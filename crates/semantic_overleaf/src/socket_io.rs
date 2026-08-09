#[cfg(test)]
use serde_json::json;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::ot::{OtError, byte_index_for_utf16, utf16_len};

const ERROR_REASONS: [&str; 3] = [
    "transport not supported",
    "client not handshaken",
    "unauthorized",
];
const ERROR_ADVICE: [&str; 1] = ["reconnect"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AckMode {
    #[default]
    None,
    Simple,
    Data,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketType {
    Disconnect,
    Connect,
    Heartbeat,
    Message,
    Json,
    Event,
    Ack,
    Error,
    Noop,
}

impl PacketType {
    fn number(self) -> usize {
        match self {
            Self::Disconnect => 0,
            Self::Connect => 1,
            Self::Heartbeat => 2,
            Self::Message => 3,
            Self::Json => 4,
            Self::Event => 5,
            Self::Ack => 6,
            Self::Error => 7,
            Self::Noop => 8,
        }
    }

    fn from_number(number: usize) -> Option<Self> {
        Some(match number {
            0 => Self::Disconnect,
            1 => Self::Connect,
            2 => Self::Heartbeat,
            3 => Self::Message,
            4 => Self::Json,
            5 => Self::Event,
            6 => Self::Ack,
            7 => Self::Error,
            8 => Self::Noop,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Packet {
    pub packet_type: PacketType,
    pub id: Option<String>,
    pub ack: AckMode,
    pub endpoint: String,
    pub data: Option<Value>,
    pub event: Option<String>,
    pub args: Vec<Value>,
    pub ack_id: Option<String>,
    pub reason: Option<String>,
    pub advice: Option<String>,
    pub query: Option<String>,
}

impl Packet {
    pub fn new(packet_type: PacketType) -> Self {
        Self {
            packet_type,
            id: None,
            ack: AckMode::None,
            endpoint: String::new(),
            data: None,
            event: None,
            args: Vec::new(),
            ack_id: None,
            reason: None,
            advice: None,
            query: None,
        }
    }

    pub fn event(name: impl Into<String>, args: Vec<Value>) -> Self {
        let mut packet = Self::new(PacketType::Event);
        packet.event = Some(name.into());
        packet.args = args;
        packet
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SocketIoCodecError {
    #[error("invalid Socket.IO 0.9 packet: {0}")]
    InvalidPacket(String),
    #[error("unknown Socket.IO 0.9 packet type: {0}")]
    UnknownPacketType(usize),
    #[error("invalid Socket.IO 0.9 JSON payload: {0}")]
    InvalidJson(String),
    #[error("invalid Socket.IO 0.9 payload framing")]
    InvalidFraming,
    #[error(transparent)]
    Utf16(#[from] OtError),
}

fn json_text(value: &Value) -> Result<String, SocketIoCodecError> {
    let encoded = serde_json::to_string(value)
        .map_err(|error| SocketIoCodecError::InvalidJson(error.to_string()))?;
    Ok(escape_non_bmp_json(&encoded))
}

// Overleaf's production Socket.IO 0.9 endpoint still passes event payloads
// through a legacy JavaScript JSON path which preserves BMP characters but
// replaces raw four-byte UTF-8 scalars with one replacement character per
// UTF-16 surrogate. JSON unicode escapes cross that boundary losslessly and
// are decoded back into the original scalar by JSON.parse on the server.
fn escape_non_bmp_json(encoded: &str) -> String {
    use std::fmt::Write as _;

    if !encoded.chars().any(|character| character.len_utf16() == 2) {
        return encoded.to_owned();
    }

    let mut escaped = String::with_capacity(encoded.len());
    for character in encoded.chars() {
        let codepoint = character as u32;
        if codepoint <= 0xffff {
            escaped.push(character);
            continue;
        }

        let surrogate = codepoint - 0x1_0000;
        let high = 0xd800 + (surrogate >> 10);
        let low = 0xdc00 + (surrogate & 0x3ff);
        write!(&mut escaped, "\\u{high:04x}\\u{low:04x}")
            .expect("writing JSON escapes into a String cannot fail");
    }
    escaped
}

pub fn encode_packet(packet: &Packet) -> Result<String, SocketIoCodecError> {
    let payload = match packet.packet_type {
        PacketType::Error => {
            let reason = packet.reason.as_deref().and_then(|value| {
                ERROR_REASONS
                    .iter()
                    .position(|candidate| *candidate == value)
            });
            let advice = packet.advice.as_deref().and_then(|value| {
                ERROR_ADVICE
                    .iter()
                    .position(|candidate| *candidate == value)
            });
            match (reason, advice) {
                (None, None) => None,
                (reason, advice) => Some(format!(
                    "{}{}",
                    reason.map(|value| value.to_string()).unwrap_or_default(),
                    advice.map(|value| format!("+{value}")).unwrap_or_default()
                )),
            }
        }
        PacketType::Message => packet
            .data
            .as_ref()
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        PacketType::Event => {
            let name = packet.event.as_deref().ok_or_else(|| {
                SocketIoCodecError::InvalidPacket("event packet has no name".into())
            })?;
            let mut object = Map::from_iter([("name".into(), Value::String(name.into()))]);
            if !packet.args.is_empty() {
                object.insert("args".into(), Value::Array(packet.args.clone()));
            }
            Some(json_text(&Value::Object(object))?)
        }
        PacketType::Json => packet.data.as_ref().map(json_text).transpose()?,
        PacketType::Connect => packet.query.clone(),
        PacketType::Ack => {
            let ack_id = packet.ack_id.as_deref().ok_or_else(|| {
                SocketIoCodecError::InvalidPacket("ack packet has no acknowledgement id".into())
            })?;
            if packet.args.is_empty() {
                Some(ack_id.to_owned())
            } else {
                Some(format!(
                    "{ack_id}+{}",
                    json_text(&Value::Array(packet.args.clone()))?
                ))
            }
        }
        _ => None,
    };

    let id = packet.id.as_deref().unwrap_or_default();
    let id_field = if packet.ack == AckMode::Data {
        format!("{id}+")
    } else {
        id.to_owned()
    };
    let mut encoded = format!(
        "{}:{}:{}",
        packet.packet_type.number(),
        id_field,
        packet.endpoint
    );
    if let Some(payload) = payload {
        encoded.push(':');
        encoded.push_str(&payload);
    }
    Ok(encoded)
}

pub fn decode_packet(encoded: &str) -> Result<Packet, SocketIoCodecError> {
    let mut fields = encoded.splitn(4, ':');
    let type_field = fields
        .next()
        .ok_or_else(|| SocketIoCodecError::InvalidPacket(encoded.into()))?;
    let id_field = fields
        .next()
        .ok_or_else(|| SocketIoCodecError::InvalidPacket(encoded.into()))?;
    let endpoint = fields
        .next()
        .ok_or_else(|| SocketIoCodecError::InvalidPacket(encoded.into()))?;
    let payload = fields.next().unwrap_or_default();
    let type_number = type_field
        .parse::<usize>()
        .map_err(|_| SocketIoCodecError::InvalidPacket(encoded.into()))?;
    let packet_type = PacketType::from_number(type_number)
        .ok_or(SocketIoCodecError::UnknownPacketType(type_number))?;
    let (id, ack) = if let Some(id) = id_field.strip_suffix('+') {
        ((!id.is_empty()).then(|| id.to_owned()), AckMode::Data)
    } else if id_field.is_empty() {
        (None, AckMode::None)
    } else {
        (Some(id_field.to_owned()), AckMode::Simple)
    };

    let mut packet = Packet::new(packet_type);
    packet.id = id;
    packet.ack = ack;
    packet.endpoint = endpoint.to_owned();
    match packet_type {
        PacketType::Error => {
            let (reason, advice) = payload.split_once('+').unwrap_or((payload, ""));
            packet.reason = reason
                .parse::<usize>()
                .ok()
                .and_then(|index| ERROR_REASONS.get(index))
                .map(|value| (*value).into());
            packet.advice = advice
                .parse::<usize>()
                .ok()
                .and_then(|index| ERROR_ADVICE.get(index))
                .map(|value| (*value).into());
        }
        PacketType::Message => packet.data = Some(Value::String(payload.into())),
        PacketType::Event => {
            let value: Value = serde_json::from_str(payload)
                .map_err(|error| SocketIoCodecError::InvalidJson(error.to_string()))?;
            packet.event = value.get("name").and_then(Value::as_str).map(str::to_owned);
            packet.args = value
                .get("args")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
        }
        PacketType::Json => {
            packet.data = Some(
                serde_json::from_str(payload)
                    .map_err(|error| SocketIoCodecError::InvalidJson(error.to_string()))?,
            );
        }
        PacketType::Connect => packet.query = (!payload.is_empty()).then(|| payload.into()),
        PacketType::Ack => {
            let digit_count = payload.bytes().take_while(u8::is_ascii_digit).count();
            if digit_count == 0 {
                return Err(SocketIoCodecError::InvalidPacket(encoded.into()));
            }
            packet.ack_id = Some(payload[..digit_count].into());
            let remaining = &payload[digit_count..];
            if let Some(json_payload) = remaining.strip_prefix('+')
                && !json_payload.is_empty()
            {
                packet.args = serde_json::from_str(json_payload)
                    .map_err(|error| SocketIoCodecError::InvalidJson(error.to_string()))?;
            }
        }
        _ => {}
    }
    Ok(packet)
}

/// Decode a single packet or the legacy `�length�packet` payload envelope.
pub fn decode_payload(payload: &str) -> Result<Vec<Packet>, SocketIoCodecError> {
    const FRAME_MARKER: char = '\u{fffd}';
    if !payload.starts_with(FRAME_MARKER) {
        return Ok(vec![decode_packet(payload)?]);
    }

    let mut remaining = &payload[FRAME_MARKER.len_utf8()..];
    let mut packets = Vec::new();
    while !remaining.is_empty() {
        let separator = remaining
            .find(FRAME_MARKER)
            .ok_or(SocketIoCodecError::InvalidFraming)?;
        let length = remaining[..separator]
            .parse::<usize>()
            .map_err(|_| SocketIoCodecError::InvalidFraming)?;
        remaining = &remaining[separator + FRAME_MARKER.len_utf8()..];
        let end = byte_index_for_utf16(remaining, length)?;
        packets.push(decode_packet(&remaining[..end])?);
        remaining = &remaining[end..];
        if let Some(next) = remaining.strip_prefix(FRAME_MARKER) {
            remaining = next;
        } else if !remaining.is_empty() {
            return Err(SocketIoCodecError::InvalidFraming);
        }
    }
    Ok(packets)
}

/// Encode the polling transport's legacy multi-packet envelope.
pub fn encode_payload(packets: &[Packet]) -> Result<String, SocketIoCodecError> {
    if packets.len() == 1 {
        return encode_packet(&packets[0]);
    }
    let mut output = String::new();
    for packet in packets {
        let encoded = encode_packet(packet)?;
        output.push('\u{fffd}');
        output.push_str(&utf16_len(&encoded).to_string());
        output.push('\u{fffd}');
        output.push_str(&encoded);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_packet_round_trips_with_data_ack() {
        let mut packet = Packet::event("applyOtUpdate", vec![json!("doc-1"), json!({ "v": 7 })]);
        packet.id = Some("4".into());
        packet.ack = AckMode::Data;
        let encoded = encode_packet(&packet).unwrap();
        assert_eq!(
            encoded,
            "5:4+::{\"name\":\"applyOtUpdate\",\"args\":[\"doc-1\",{\"v\":7}]}"
        );
        assert_eq!(decode_packet(&encoded).unwrap(), packet);
    }

    #[test]
    fn event_packet_escapes_non_bmp_json_as_surrogate_pairs() {
        let packet = Packet::event("applyOtUpdate", vec![json!("한😀")]);
        let encoded = encode_packet(&packet).unwrap();
        assert!(encoded.contains("한\\ud83d\\ude00"));
        assert!(!encoded.contains('😀'));
        assert_eq!(decode_packet(&encoded).unwrap(), packet);
    }

    #[test]
    fn acknowledgement_packet_round_trips() {
        let mut packet = Packet::new(PacketType::Ack);
        packet.ack_id = Some("12".into());
        packet.args = vec![Value::Null, json!({ "ok": true })];
        let encoded = encode_packet(&packet).unwrap();
        assert_eq!(encoded, "6:::12+[null,{\"ok\":true}]");
        assert_eq!(decode_packet(&encoded).unwrap(), packet);
    }

    #[test]
    fn heartbeat_has_exact_legacy_wire_format() {
        assert_eq!(
            encode_packet(&Packet::new(PacketType::Heartbeat)).unwrap(),
            "2::"
        );
    }

    #[test]
    fn framed_payload_uses_javascript_utf16_lengths() {
        let packets = vec![
            Packet::event("message", vec![json!("한😀")]),
            Packet::new(PacketType::Heartbeat),
        ];
        let encoded = encode_payload(&packets).unwrap();
        assert!(encoded.starts_with('\u{fffd}'));
        assert_eq!(decode_payload(&encoded).unwrap(), packets);
    }

    #[test]
    fn error_reason_and_reconnect_advice_round_trip() {
        let mut packet = Packet::new(PacketType::Error);
        packet.reason = Some("unauthorized".into());
        packet.advice = Some("reconnect".into());
        let encoded = encode_packet(&packet).unwrap();
        assert_eq!(encoded, "7:::2+0");
        assert_eq!(decode_packet(&encoded).unwrap(), packet);
    }
}
