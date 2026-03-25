//! Vector serialization for CQL VECTOR<float, N> type.
//!
//! cdrs-tokio v9 doesn't support the VECTOR type (type ID 0x0023).
//! We work around this by serializing Vec<f32> to raw bytes (Blob)
//! and deserializing back. The CQL wire format for VECTOR<float, N>
//! is N consecutive big-endian IEEE 754 f32 values.

/// Serialize a Vec<f32> to CQL VECTOR wire format (big-endian f32 bytes).
pub fn encode_vector(values: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(values.len() * 4);
    for &v in values {
        buf.extend_from_slice(&v.to_be_bytes());
    }
    buf
}

/// Deserialize CQL VECTOR wire format to Vec<f32>.
pub fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let original = vec![0.1, 0.2, 0.3, 0.4];
        let encoded = encode_vector(&original);
        assert_eq!(encoded.len(), 16); // 4 floats * 4 bytes
        let decoded = decode_vector(&encoded);
        for (a, b) in original.iter().zip(decoded.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn encode_empty() {
        assert!(encode_vector(&[]).is_empty());
    }

    #[test]
    fn decode_empty() {
        assert!(decode_vector(&[]).is_empty());
    }

    #[test]
    fn encode_768_dimensions() {
        let embedding: Vec<f32> = (0..768).map(|i| i as f32 / 768.0).collect();
        let encoded = encode_vector(&embedding);
        assert_eq!(encoded.len(), 768 * 4);
        let decoded = decode_vector(&encoded);
        assert_eq!(decoded.len(), 768);
        for (a, b) in embedding.iter().zip(decoded.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn handles_special_values() {
        let values = vec![0.0, -0.0, f32::INFINITY, f32::NEG_INFINITY];
        let encoded = encode_vector(&values);
        let decoded = decode_vector(&encoded);
        assert_eq!(decoded[0], 0.0);
        assert!(decoded[2].is_infinite() && decoded[2] > 0.0);
        assert!(decoded[3].is_infinite() && decoded[3] < 0.0);
    }
}
