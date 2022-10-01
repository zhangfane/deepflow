/*
 * Copyright (c) 2022 Yunshan Networks
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::net::IpAddr;

use enum_dispatch::enum_dispatch;
use public::enums::IpProtocol;
use public::l7_protocol::L7Protocol;

use super::ebpf::EbpfType;
use super::flow::PacketDirection;
use super::l7_protocol_info::L7ProtocolInfo;
use super::MetaPacket;

use crate::config::handler::LogParserAccess;
use crate::flow_generator::protocol_logs::{
    DnsLog, DubboLog, HttpLog, KafkaLog, MqttLog, MysqlLog, PostgresqlLog, RedisLog,
};
use crate::flow_generator::Result;

/*
 所有协议都需要实现L7ProtocolLogInterface这个接口.
 其中，check_payload 用于MetaPacket判断应用层协议，parse_payload 用于解析具体协议.
 更具体就是遍历ALL_PROTOCOL的协议，用check判断协议，再用parse解析整个payload，得到L7ProtocolInfo.
 最后发送到server之前，调用into() 转成通用结构L7ProtocolSendLog.

 all protocol need implement L7ProtocolLogInterface trait.
 check_payload use to determine what protocol the payload is.
 parse_payload use to parse whole payload.
 more specifically, traversal all protocol from get_all_protocol,check the payload and then parse it,
 get the L7ProtocolInfo enum, finally convert to L7ProtocolSendLog struct and send to server.


 ebpf处理过程为:

 ebpf protocol parse process:


                                   payload:&[u8]
                                         |
                                         |
                                    MetaPacket
                                         |
                                         |
                                         |
                                      check()
                                         |
                                         |
                           traversal all implement protocol
                                         |
                                         |
                                         |
                        L7ProtocolParser::check_payload()
                                  |           |
                                  |           |
                                  v           v
                     <-----------true       false-------->set protocol as unknown, then ignore the packet.
                    |
                    |
                    v
       L7ProtocolParser::parse_payload()
                    |
                    |
         |<---------v---------->Vec<L7ProtocolInfo>-------->
         |                                                 |
         v                                                 |
 raise err, ignore the packet                              v
                                                      for each info
                                                           |
                                                           |
                                                           v
                           find the req/resp in SessionAggr, only find 2 solt(include current solt)
                                                           |
                                                           |
                           | <------found req/resp <------ v------> not found ------->save in current slot , wait req/resp
                           |
                           |
                           v
             L7ProtocolInfo::merge_log(req/resp)   (merge req and resp to session)
                           |
                           |
                           v
               ! L7ProtocolInfo::skip_send()
                           |
                           v
                    send to server


about SessionAggr:

    [
        hashmap< key = u64, value = L7protocolLog >, (represent 60s)
        hashmap< key = u64, value = L7protocolLog >, (represent 60s)
        ....
    ]

    it is time slot array(default length 16) + hashmap struct, every time slot repersent 60s time.

    key is u64 : | flow_id hight 8 bit | flow_id low 24 bit | proto 8 bit | session low 24 bit |

    flow_id: from ebpf socket_id, distinguish the socket fd.
    proto:   protocol number, such as: tcp=6 udp=17
    session: depend on the protocol, for example http2:stream_id,  dns:transaction_id.



about check():
    check() will travsal all protocol from get_all_protocol() to determine what protocol belong to the payload.
    first, check the bitmap(describe follow) is ignore the protocol ? then call L7ProtocolParser::check_payload() to check.

about parse_payload()
    it use same struct in L7ProtocolParser::check_payload().

about bitmap:
    u128, every bit repersent the protocol shoud check or not(1 indicate check, 0 for ignore), the number of protocol as follow:

    Http1 = 20,
    Http2 = 21,
    Http1TLS = 22,
    Http2TLS = 23,
    Dubbo = 40,
    Mysql = 60,
    Postgresql = 61,
    Redis = 80,
    Kafka = 100,
    Mqtt = 101,
    Dns = 120,

 TODO: cbpf 处理过程
 hint: check 和 parse 是同一个结构，check可以把解析结果保存下来,避免重复解析.
       check and parse use same struct. so it can save information and prevent duplicate parse.

*/

#[macro_export]
macro_rules! parse_common {
    ($self:ident,$parse_param:ident) => {
        $self.info.start_time = $parse_param.time;
        $self.info.end_time = $parse_param.time;
        if let Some(param) = $parse_param.ebpf_param {
            $self.info.is_tls = param.is_tls;
        }
    };
}

macro_rules! all_protocol {
    ($( $l7_proto:ident , $parser:ident , $log:ident::$new_func:ident);+$(;)?) => {
        #[enum_dispatch]
        pub enum L7ProtocolParser {
            HttpParser(HttpLog),

            $(
                $parser($log),
            )+
        }

        pub fn get_parser(p: L7Protocol) -> Option<L7ProtocolParser> {
            match p {
                L7Protocol::Http1 => Some(L7ProtocolParser::HttpParser(HttpLog::new_v1())),
                L7Protocol::Http2 => Some(L7ProtocolParser::HttpParser(HttpLog::new_v2())),

                $(
                    L7Protocol::$l7_proto=>Some(L7ProtocolParser::$parser($log::$new_func())),
                )+
                _=>None,
            }

        }

        pub fn get_all_protocol() -> Vec<L7ProtocolParser> {
            Vec::from([
                L7ProtocolParser::HttpParser(HttpLog::new_v1()),
                L7ProtocolParser::HttpParser(HttpLog::new_v2()),

                $(
                    L7ProtocolParser::$parser($log::$new_func()),
                )+
            ])
        }
    };
}

/*
macro expand result like:

#[enum_dispatch]
pub enum L7ProtocolParser {
    HttpParser(HttpLog),
    DnsParser(DnsLog),
    MysqlParser(MysqlLog),
    ...
}

pub fn get_parser(p: L7Protocol) -> Option<L7ProtocolParser> {
    match p {
        L7Protocol::Http1 => Some(L7ProtocolParser::HttpParser(HttpLog::new_v1())),
        L7Protocol::Http2 => Some(L7ProtocolParser::HttpParser(HttpLog::new_v2())),
        L7Protocol::Dns => Some(L7ProtocolParser::DnsParser(DnsLog::default())),
        L7Protocol::Mysql => Some(L7ProtocolParser::MysqlParser(MysqlLog::default())),
        ...

    }
}

pub fn get_all_protocol() -> Vec<L7ProtocolParser> {
    Vec::from([
        L7ProtocolParser::HttpParser(HttpLog::new_v1()),
        L7ProtocolParser::HttpParser(HttpLog::new_v2()),
        L7ProtocolParser::DnsParser(DnsLog::default()),
        L7ProtocolParser::MysqlParser(MysqlLog::default()),
        ...
    ])

}

*/

// 内部实现的协议
// log的具体结构和实现在 src/flow_generator/protocol_logs/** 下
// =========================================================
// the inner implement protocol source code in src/flow_generator/protocol_logs/**

// l7Protocol , enumName , ParserImplement::newFuncName
all_protocol!(
    // http have two version but one parser, can not place in macro param.
    Dns,DnsParser,DnsLog::default;
    Mysql,MysqlParser,MysqlLog::default;
    Kafka,KafkaParser,KafkaLog::default;
    Redis,RedisParser,RedisLog::default;
    Postgresql,PostgresParser,PostgresqlLog::default;
    Dubbo,DubboParser,DubboLog::default;
    Mqtt,MqttParser,MqttLog::default;
    // add protocol below
);

impl L7ProtocolParser {
    pub fn is_skip_parse(&self, bitmap: u128) -> bool {
        bitmap & (1 << (self.protocol() as u8)) == 0
    }

    pub fn set_bitmap_skip_parse(&self, bitmap: &mut u128) {
        *bitmap &= !(1 << (self.protocol() as u8));
    }
}

#[enum_dispatch(L7ProtocolParser)]
pub trait L7ProtocolParserInterface {
    fn check_payload(&mut self, payload: &[u8], param: &ParseParam) -> bool;
    // 协议解析
    fn parse_payload(&mut self, payload: &[u8], param: &ParseParam) -> Result<Vec<L7ProtocolInfo>>;
    // 返回协议号和协议名称，由于的bitmap使用u128，所以协议号不能超过128.
    // 其中 crates/public/src/l7_protocol.rs 里面的 pub const L7_PROTOCOL_xxx 是已实现的协议号.
    // ===========================================================================================
    // return protocol number and protocol string. because of bitmap use u128, so the max protocol number can not exceed 128
    // crates/public/src/l7_protocol.rs, pub const L7_PROTOCOL_xxx is the implemented protocol.
    fn protocol(&self) -> L7Protocol;
    // 仅http和dubbo协议会有log_parser_config，其他协议可以忽略。
    // ================================================================
    // only http and dubbo use config. other protocol should do nothing.
    fn set_parse_config(&mut self, _log_parser_config: &LogParserAccess) {}
    // l4是tcp时是否解析，用于快速过滤协议
    // ==============================
    // whether l4 is parsed when tcp, use for quickly protocol filter
    fn parsable_on_tcp(&self) -> bool {
        true
    }
    // l4是udp是是否解析，用于快速过滤协议
    // ==============================
    // whether l4 is parsed when udp, use for quickly protocol filter
    fn parsable_on_udp(&self) -> bool {
        true
    }
    fn reset(&mut self);
}

#[derive(Clone, Copy)]
pub struct EbpfParam {
    pub is_tls: bool,
    // 目前仅 http2 uprobe 有意义
    // ==========================
    // now only http2 uprobe uses
    pub is_req_end: bool,
    pub is_resp_end: bool,
}

#[derive(Clone, Copy)]
pub struct ParseParam {
    // l3/l4 info
    pub l4_protocol: IpProtocol,
    pub ip_src: IpAddr,
    pub ip_dst: IpAddr,
    pub port_src: u16,
    pub port_dst: u16,

    pub direction: PacketDirection,
    pub ebpf_type: EbpfType,
    // ebpf_type 不为 EBPF_TYPE_NONE 会有值
    // ===================================
    // not None when payload from ebpf
    pub ebpf_param: Option<EbpfParam>,
    pub time: u64,
}

impl From<&MetaPacket<'_>> for ParseParam {
    fn from(packet: &MetaPacket<'_>) -> Self {
        let mut param = Self {
            l4_protocol: packet.lookup_key.proto,
            ip_src: packet.lookup_key.src_ip,
            ip_dst: packet.lookup_key.dst_ip,
            port_src: packet.lookup_key.src_port,
            port_dst: packet.lookup_key.dst_port,

            direction: packet.direction,
            ebpf_type: packet.ebpf_type,
            ebpf_param: None,
            time: packet.start_time.as_micros() as u64,
        };
        if packet.ebpf_type != EbpfType::None {
            let is_tls = match packet.ebpf_type {
                EbpfType::TlsUprobe => true,
                _ => match packet.l7_protocol_from_ebpf {
                    L7Protocol::Http1TLS | L7Protocol::Http2TLS => true,
                    _ => false,
                },
            };
            param.ebpf_param = Some(EbpfParam {
                is_tls,
                is_req_end: packet.is_request_end,
                is_resp_end: packet.is_response_end,
            });
        }

        param
    }
}

pub fn get_bitmap(protocol: IpProtocol) -> u128 {
    let mut bitmap: u128 = 0;
    for i in get_all_protocol().iter() {
        match protocol {
            IpProtocol::Tcp if i.parsable_on_tcp() => {
                bitmap |= 1 << (i.protocol() as u8);
            }
            IpProtocol::Udp if i.parsable_on_udp() => {
                bitmap |= 1 << (i.protocol() as u8);
            }
            _ => {}
        }
    }

    bitmap
}