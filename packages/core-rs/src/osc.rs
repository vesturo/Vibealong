#[derive(Debug, Clone, PartialEq)]
pub enum OscArg {
    Int(i32),
    Float(f32),
    Bool(bool),
    Str(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OscMessage {
    pub address: String,
    pub args: Vec<OscArg>,
    pub arg_type: Option<char>,
    pub arg_types: String,
}

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

fn read_osc_string(packet: &[u8], offset: usize) -> Option<(String, usize)> {
    let mut cursor = offset;
    while cursor < packet.len() && packet[cursor] != 0 {
        cursor += 1;
    }
    if cursor >= packet.len() {
        return None;
    }
    let value = String::from_utf8_lossy(&packet[offset..cursor]).to_string();
    let next_offset = align4(cursor + 1);
    if next_offset > packet.len() {
        return None;
    }
    Some((value, next_offset))
}

fn read_i32_be(packet: &[u8], offset: usize) -> i32 {
    i32::from_be_bytes([
        packet[offset],
        packet[offset + 1],
        packet[offset + 2],
        packet[offset + 3],
    ])
}

fn read_f32_be(packet: &[u8], offset: usize) -> f32 {
    f32::from_bits(u32::from_be_bytes([
        packet[offset],
        packet[offset + 1],
        packet[offset + 2],
        packet[offset + 3],
    ]))
}

fn parse_osc_message(packet: &[u8], offset: usize) -> Option<OscMessage> {
    let (address, next_after_address) = read_osc_string(packet, offset)?;
    let (type_tag, mut cursor) = read_osc_string(packet, next_after_address)?;
    if !type_tag.starts_with(',') {
        return None;
    }

    let mut args = Vec::new();
    for arg_type in type_tag.chars().skip(1) {
        match arg_type {
            'i' => {
                if cursor + 4 > packet.len() {
                    return None;
                }
                args.push(OscArg::Int(read_i32_be(packet, cursor)));
                cursor += 4;
            }
            'f' => {
                if cursor + 4 > packet.len() {
                    return None;
                }
                args.push(OscArg::Float(read_f32_be(packet, cursor)));
                cursor += 4;
            }
            'T' => args.push(OscArg::Bool(true)),
            'F' => args.push(OscArg::Bool(false)),
            's' => {
                let (value, next_cursor) = read_osc_string(packet, cursor)?;
                args.push(OscArg::Str(value));
                cursor = next_cursor;
            }
            _ => return None,
        }
    }

    let arg_types: String = type_tag.chars().skip(1).collect();
    let arg_type = arg_types.chars().next();
    Some(OscMessage {
        address,
        args,
        arg_type,
        arg_types,
    })
}

pub fn parse_osc_packet(packet: &[u8]) -> Vec<OscMessage> {
    let Some((first, mut cursor)) = read_osc_string(packet, 0) else {
        return Vec::new();
    };

    if first != "#bundle" {
        return parse_osc_message(packet, 0).into_iter().collect();
    }

    if cursor + 8 > packet.len() {
        return Vec::new();
    }
    cursor += 8;

    let mut messages = Vec::new();
    while cursor + 4 <= packet.len() {
        let size = read_i32_be(packet, cursor);
        cursor += 4;
        if size <= 0 {
            break;
        }
        let size = size as usize;
        if cursor + size > packet.len() {
            break;
        }
        messages.extend(parse_osc_packet(&packet[cursor..cursor + size]));
        cursor += size;
    }

    messages
}

pub fn extract_numeric_arg(args: &[OscArg]) -> Option<f64> {
    for arg in args {
        match arg {
            OscArg::Int(value) => return Some(*value as f64),
            OscArg::Float(value) if value.is_finite() => return Some(*value as f64),
            OscArg::Bool(value) => return Some(if *value { 1.0 } else { 0.0 }),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_osc_string(buf: &mut Vec<u8>, value: &str) {
        buf.extend_from_slice(value.as_bytes());
        buf.push(0);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
    }

    fn osc_message_float(address: &str, value: f32) -> Vec<u8> {
        let mut msg = Vec::new();
        write_osc_string(&mut msg, address);
        write_osc_string(&mut msg, ",f");
        msg.extend_from_slice(&value.to_bits().to_be_bytes());
        msg
    }

    fn osc_message_unsupported(address: &str) -> Vec<u8> {
        let mut msg = Vec::new();
        write_osc_string(&mut msg, address);
        write_osc_string(&mut msg, ",d");
        msg
    }

    fn bundle(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut packet = Vec::new();
        write_osc_string(&mut packet, "#bundle");
        packet.extend_from_slice(&0_u64.to_be_bytes());
        for chunk in chunks {
            packet.extend_from_slice(&(chunk.len() as i32).to_be_bytes());
            packet.extend_from_slice(chunk);
        }
        packet
    }

    #[test]
    fn parses_single_message() {
        let packet = osc_message_float("/avatar/parameters/SPS_Contact", 0.5);
        let messages = parse_osc_packet(&packet);
        assert_eq!(messages.len(), 1);
        let first = &messages[0];
        assert_eq!(first.address, "/avatar/parameters/SPS_Contact");
        assert_eq!(first.arg_type, Some('f'));
        assert_eq!(first.arg_types, "f");
        assert_eq!(first.args, vec![OscArg::Float(0.5)]);
    }

    #[test]
    fn parses_bundle_messages() {
        let one = osc_message_float("/a", 0.25);
        let two = osc_message_float("/b", 0.75);
        let packet = bundle(&[one, two]);
        let messages = parse_osc_packet(&packet);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].address, "/a");
        assert_eq!(messages[1].address, "/b");
    }

    #[test]
    fn unsupported_type_discards_message() {
        let packet = osc_message_unsupported("/bad");
        let messages = parse_osc_packet(&packet);
        assert!(messages.is_empty());
    }

    #[test]
    fn bundle_keeps_valid_messages_before_invalid_chunk() {
        let valid = osc_message_float("/ok", 1.0);
        let mut packet = bundle(&[valid.clone()]);
        packet.extend_from_slice(&(999_i32).to_be_bytes());
        packet.extend_from_slice(&[1, 2, 3, 4]);
        let messages = parse_osc_packet(&packet);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].address, "/ok");
    }

    #[test]
    fn numeric_extraction_matches_js_order() {
        let args = vec![
            OscArg::Str("x".to_string()),
            OscArg::Bool(true),
            OscArg::Float(0.3),
        ];
        let out = extract_numeric_arg(&args);
        assert_eq!(out, Some(1.0));
    }
}
