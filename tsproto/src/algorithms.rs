//! Handle packet splitting and cryptography
use std::u64;

use byteorder::{NetworkEndian, WriteBytesExt};
use curve25519_dalek::edwards::EdwardsPoint;
use num::bigint::BigUint;
use quicklz::CompressionLevel;
use ring::digest;

use {crypto, Result};
use connection::{CachedKey, SharedIv};
use crypto::{EccKeyPrivEd25519, EccKeyPrivP256, EccKeyPubP256};
use packets::*;

pub fn must_encrypt(t: PacketType) -> bool {
    match t {
        PacketType::Command | PacketType::CommandLow => true,
        PacketType::Voice |
        PacketType::Ack |
        PacketType::AckLow |
        PacketType::VoiceWhisper |
        PacketType::Ping |
        PacketType::Pong |
        PacketType::Init => false,
    }
}

pub fn should_encrypt(t: PacketType, voice_encryption: bool) -> bool {
    must_encrypt(t) || t == PacketType::Ack || t == PacketType::AckLow
        || (voice_encryption && t.is_voice())
}

/// Compresses and splits the packet data of a `Command` or `CommandLow` packet.
///
/// Returns the splitted packet data and their headers.
/// The headers have their type and the compressed and fragmented flag set
/// to the right value.
///
/// Returns an error if the packet is too large but cannot be splitted.
/// Only `Command` and `CommandLow` packets can be compressed and splitted.
pub fn compress_and_split(is_client: bool, packet: &Packet)
    -> Vec<(Header, Vec<u8>)> {
    // Everything else (except whisper packets) have to be less than 500 bytes
    let header_size = if is_client { 13 } else { 11 };
    let mut data = Vec::new();
    packet.data.write(&mut data).unwrap();
    // The maximum packet size (including header) is 500 bytes.
    let max_size = 500 - header_size;
    // Split the data if it is necessary.
    // Compress also slightly smaller packets
    let (datas, compressed) = if data.len() > (max_size - 100) {
        // Compress with QuickLZ
        let cdata = ::quicklz::compress(&data, CompressionLevel::Lvl1);
        // Use only if it is efficient
        let (mut data, compressed) = if cdata.len() > data.len() {
            (data, false)
        } else {
            (cdata, true)
        };

        // Ignore size limit for whisper packets
        if data.len() <= max_size
            || packet.header.get_type() == PacketType::VoiceWhisper
        {
            (vec![data], compressed)
        } else {
            // Split
            let count = (data.len() + max_size - 1) / max_size;
            let mut splitted = Vec::with_capacity(count);
            // Split from the back so the buffer does not have to be moved each
            // time.
            // Rest
            let mut len = data.len();
            splitted.push(data.split_off(len - (len % max_size)));

            while {
                len = data.len();
                len > 0
            } {
                splitted.push(data.split_off(len - max_size));
            }
            (splitted, compressed)
        }
    } else {
        (vec![data], false)
    };
    let len = datas.len();
    let fragmented = len > 1;
    let default_header = {
        let mut h = Header::default();
        h.set_type(packet.header.get_type());
        h
    };
    let mut packets = Vec::with_capacity(datas.len());
    for (i, d) in datas.into_iter().rev().enumerate() {
        let mut h = default_header.clone();
        // Only set flags on first fragment
        if i == 0 && compressed {
            h.set_compressed(true);
        }

        // Set fragmented flag on first and last part
        if fragmented && (i == 0 || i == len - 1) {
            h.set_fragmented(true);
        }
        packets.push((h, d));
    }
    packets
}

fn create_key_nonce(
    header: &Header,
    generation_id: u32,
    iv: &SharedIv,
    cache: &mut [CachedKey; 8],
) -> ([u8; 16], [u8; 16]) {
    // Check if this generation is cached
    let cache = &mut cache[(header.p_type & 0xf) as usize];
    if cache.generation_id != generation_id {
        // Update the cache
        let mut temp = [0; 70];
        if header.c_id.is_some() {
            temp[0] = 0x31;
        } else {
            temp[0] = 0x30;
        }
        temp[1] = header.p_type & 0xf;
        let mut buf = Vec::with_capacity(4);
        buf.write_u32::<NetworkEndian>(generation_id).unwrap();
        temp[2..6].copy_from_slice(&buf);
        let len;
        match *iv {
            SharedIv::ProtocolOrig(ref data) => {
                temp[6..26].copy_from_slice(data);
                len = 26;
            }
            SharedIv::Protocol31(ref data) => {
                temp[6..].copy_from_slice(data);
                len = 70;
            }
        }

        let keynonce = digest::digest(&digest::SHA256, &temp[..len]);
        let keynonce = keynonce.as_ref();
        cache.key.copy_from_slice(&keynonce[..16]);
        cache.nonce.copy_from_slice(&keynonce[16..]);
    }

    // Use the cached version
    let mut key = cache.key;
    let nonce = cache.nonce;
    key[0] ^= (header.p_id >> 8) as u8;
    key[1] ^= (header.p_id & 0xff) as u8;
    (key, nonce)
}

pub fn encrypt_key_nonce(
    header: &mut Header,
    data: &mut [u8],
    key: &[u8; 16],
    nonce: &[u8; 16],
) -> Result<()> {
    let mut meta = Vec::with_capacity(5);
    header.write_meta(&mut meta)?;

    let (mac, enc) = crypto::Eax::encrypt(key, nonce, &meta, data)?;
    header.mac.copy_from_slice(&mac[..8]);
    data.copy_from_slice(&enc);

    Ok(())
}

pub fn encrypt_fake(header: &mut Header, data: &mut [u8]) -> Result<()> {
    encrypt_key_nonce(header, data, &::FAKE_KEY, &::FAKE_NONCE)
}

pub fn encrypt(
    header: &mut Header,
    data: &mut [u8],
    generation_id: u32,
    iv: &SharedIv,
    cache: &mut [CachedKey; 8],
) -> Result<()> {
    let (key, nonce) = create_key_nonce(header, generation_id, iv, cache);
    encrypt_key_nonce(header, data, &key, &nonce)
}

pub fn decrypt_key_nonce(
    header: &Header,
    data: &mut [u8],
    key: &[u8; 16],
    nonce: &[u8; 16],
) -> Result<()> {
    let mut meta = Vec::with_capacity(5);
    header.write_meta(&mut meta)?;

    let dec = crypto::Eax::decrypt(key, nonce, &meta, data, &header.mac)?;
    data.copy_from_slice(&dec);
    Ok(())
}

pub fn decrypt_fake(header: &Header, data: &mut [u8]) -> Result<()> {
    decrypt_key_nonce(header, data, &::FAKE_KEY, &::FAKE_NONCE)
}

pub fn decrypt(
    header: &Header,
    data: &mut [u8],
    generation_id: u32,
    iv: &SharedIv,
    cache: &mut [CachedKey; 8],
) -> Result<()> {
    let (key, nonce) = create_key_nonce(header, generation_id, iv, cache);
    decrypt_key_nonce(header, data, &key, &nonce)
}

/// Compute shared iv and shared mac.
pub fn compute_iv_mac(
    alpha: &[u8; 10],
    beta: &[u8; 10],
    our_key: EccKeyPrivP256,
    other_key: EccKeyPubP256,
) -> Result<([u8; 20], [u8; 8])> {
    let shared_secret = our_key.create_shared_secret(other_key)?;
    let mut shared_iv = [0; 20];
    shared_iv.copy_from_slice(digest::digest(&digest::SHA1, &shared_secret)
        .as_ref());
    for i in 0..10 {
        shared_iv[i] ^= alpha[i];
    }
    for i in 0..10 {
        shared_iv[i + 10] ^= beta[i];
    }
    let mut shared_mac = [0; 8];
    shared_mac.copy_from_slice(
        &digest::digest(&digest::SHA1, &shared_iv).as_ref()[..8],
    );
    Ok((shared_iv, shared_mac))
}

pub fn compute_iv_mac31(
    alpha: &[u8; 10],
    beta: &[u8; 54],
    our_key: &EccKeyPrivEd25519,
    other_key: &EdwardsPoint,
) -> Result<([u8; 64], [u8; 8])> {
    let shared_secret = our_key.create_shared_secret(other_key)?;
    let mut shared_iv = [0; 64];
    shared_iv.copy_from_slice(digest::digest(&digest::SHA512, &shared_secret)
        .as_ref());
    for i in 0..10 {
        shared_iv[i] ^= alpha[i];
    }
    for i in 0..54 {
        shared_iv[i + 10] ^= beta[i];
    }
    let mut shared_mac = [0; 8];
    shared_mac.copy_from_slice(
        &digest::digest(&digest::SHA1, &shared_iv).as_ref()[..8],
    );
    Ok((shared_iv, shared_mac))
}


pub fn hash_cash(key: &EccKeyPubP256, level: u8) -> Result<u64> {
    let omega = key.to_ts()?;
    let mut offset = 0;
    while offset < u64::MAX && get_hash_cash_level(&omega, offset) < level {
        offset += 1;
    }
    Ok(offset)
}

pub fn get_hash_cash_level(omega: &str, offset: u64) -> u8 {
    let data = digest::digest(
        &digest::SHA1,
        format!("{}{}", omega, offset).as_bytes(),
    );
    let mut res = 0;
    for &d in data.as_ref() {
        if d == 0 {
            res += 8;
        } else {
            res += d.leading_zeros() as u8;
            break;
        }
    }
    res
}

pub fn biguint_to_array(i: &BigUint) -> [u8; 64] {
    let mut v = i.to_bytes_le();

    // Extend with zeroes until 64 bytes
    let len = v.len();
    v.append(&mut vec![0; 64 - len]);
    v.reverse();

    let mut a = [0; 64];
    a.copy_from_slice(&v);
    a
}

pub fn array_to_biguint(i: &[u8; 64]) -> BigUint {
    BigUint::from_bytes_be(i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64;
    use packets::{Data, Header, PacketType};

    #[test]
    fn test_fake_crypt() {
        ::init().unwrap();
        let data = (0..100).into_iter().collect::<Vec<_>>();
        let mut header = Header::default();
        let mut enc_data = data.clone();
        encrypt_fake(&mut header, &mut enc_data).unwrap();
        let mut dec_data = enc_data.clone();
        decrypt_fake(&header, &mut dec_data).unwrap();
        assert_eq!(&data, &dec_data);
    }

    #[test]
    fn test_fake_encrypt() {
        let data = Data::Ack(0);
        let mut p_data = Vec::new();
        data.write(&mut p_data).unwrap();
        let mut header = Header::default();
        header.c_id = Some(0);
        header.set_type(PacketType::Ack);
        encrypt_fake(&mut header, &mut p_data).unwrap();

        let mut buf = Vec::new();
        header.write(&mut buf).unwrap();
        buf.append(&mut p_data);
        let real_res: &[u8] = &[0xa4, 0x7b, 0x47, 0x94, 0xdb, 0xa9, 0x6a, 0xc5,
            0, 0, 0, 0, 0x6, 0xfe, 0x18];
        assert_eq!(real_res, buf.as_slice());
    }

    #[test]
    #[should_panic]
    fn shared_iv31() {
        let licenses = ::license::Licenses::parse(&base64::decode("AQA1hUFJiiSs\
            0wFXkYuPUJVcDa6XCrZTcsvkB0Ffzz4CmwIITRXgCqeTYAcAAAAgQW5vbnltb3VzAAC\
            4R+5mos+UQ/KCbkpQLMI5WRp4wkQu8e5PZY4zU+/FlyAJwaE8CcJJ/A==")
            .unwrap()).unwrap();
        let derived_key = licenses.derive_public_key().unwrap();

        let client_ek = [0xb0, 0x4e, 0xa1, 0xd9, 0x5c, 0x72, 0x64, 0xdf, 0x0d,
            0xe8, 0xb3, 0x6b, 0xaa, 0x7c, 0xa1, 0x5f, 0x75, 0x71, 0xf5, 0x1f,
            0xa0, 0x54, 0xb5, 0x51, 0x27, 0x08, 0x8e, 0xdd, 0x96, 0x3d, 0x6e,
            0x79];

        let priv_key = ::crypto::EccKeyPrivEd25519::from_bytes(client_ek);

        let alpha_b64 = base64::decode("Jkxq1wIvvhzaCA==").unwrap();
        let mut alpha = [0; 10];
        alpha.copy_from_slice(&alpha_b64);
        let beta_b64 = base64::decode("wU5T/MM6toW6Wge9th7VlTlzVZ9JDWypw2P9migf\
            c25pjGP2Tj7Hm6rJpmKeHRr08Ch7BEAR").unwrap();
        let mut beta = [0; 54];
        beta.copy_from_slice(&beta_b64);

        let expected_shared_shared_iv: [u8; 64] = [0x58, 0x78, 0xae, 0x08, 0x08,
            0x72, 0x05, 0xb0, 0x13, 0x27, 0x10, 0xe9, 0x81, 0xb4, 0xaf, 0x14,
            0x14, 0x71, 0xad, 0xcd, 0x82, 0x98, 0xf3, 0xd1, 0x1d, 0x07, 0x20,
            0x72, 0x7e, 0xb2, 0x1b, 0x89, 0x47, 0x82, 0x1e, 0xfb, 0x02, 0x53,
            0x5a, 0x8a, 0x52, 0x4d, 0x9a, 0x7a, 0x09, 0x2c, 0x1b, 0xe7, 0x1f,
            0xd1, 0x9d, 0x2a, 0x9d, 0x4f, 0xbd, 0xe3, 0x22, 0x09, 0xe4, 0x86,
            0x7d, 0x63, 0x49, 0x07];

        let expected_xored_shared_shared_iv: [u8; 64] = [0x7e, 0x34, 0xc4, 0xdf,
            0x0a, 0x5d, 0xbb, 0xac, 0xc9, 0x2f, 0xd1, 0xa7, 0xd2, 0x48, 0x6c,
            0x2e, 0xa2, 0xf4, 0x17, 0x97, 0x85, 0x25, 0x45, 0xcf, 0xc8, 0x92,
            0x19, 0x01, 0x2b, 0x2d, 0x52, 0x84, 0x2b, 0x2b, 0xdd, 0x98, 0xff,
            0xc9, 0x72, 0x95, 0x21, 0x23, 0xf3, 0xf6, 0x6a, 0xda, 0x55, 0xd9,
            0xd8, 0x4a, 0x37, 0xe3, 0x3b, 0x2d, 0x23, 0xfe, 0x38, 0xfd, 0x14,
            0xae, 0x06, 0x67, 0x09, 0x16];

        let (mut shared_iv, _shared_mac) = compute_iv_mac31(&alpha, &beta,
            &priv_key, &derived_key).unwrap();

        assert_eq!(&shared_iv as &[u8],
            &expected_xored_shared_shared_iv as &[u8]);

        for i in 0..10 {
            shared_iv[i] ^= alpha[i];
        }
        for i in 0..54 {
            shared_iv[i + 10] ^= beta[i];
        }

        assert_eq!(&shared_iv as &[u8], &expected_shared_shared_iv as &[u8]);

        let mut temp = [0; 70];
        temp[0] = 0x31;
        temp[1] = 0x2 & 0xf;
        temp[6..].copy_from_slice(&expected_xored_shared_shared_iv);


        let temporary: [u8; 70] = [0x31, 0x02, 0x00, 0x00, 0x00, 0x00, 0x7e,
            0x34, 0xc4, 0xdf, 0x0a, 0x5d, 0xbb, 0xac, 0xc9, 0x2f, 0xd1, 0xa7,
            0xd2, 0x48, 0x6c, 0x2e, 0xa2, 0xf4, 0x17, 0x97, 0x85, 0x25, 0x45,
            0xcf, 0xc8, 0x92, 0x19, 0x01, 0x2b, 0x2d, 0x52, 0x84, 0x2b, 0x2b,
            0xdd, 0x98, 0xff, 0xc9, 0x72, 0x95, 0x21, 0x23, 0xf3, 0xf6, 0x6a,
            0xda, 0x55, 0xd9, 0xd8, 0x4a, 0x37, 0xe3, 0x3b, 0x2d, 0x23, 0xfe,
            0x38, 0xfd, 0x14, 0xae, 0x06, 0x67, 0x09, 0x16];
        assert!(&temp as &[u8] == &temporary as &[u8]);

        let keynonce = digest::digest(&digest::SHA256, &temp);

        let expected_keynonce: [u8; 32] = [0xf3, 0x70, 0xd3, 0x43, 0xe7, 0x78,
            0x15, 0x70, 0x7a, 0xff, 0x60, 0x48, 0xfb, 0xd9, 0xac, 0x6b, 0xb6,
            0x33, 0x35, 0x79, 0x31, 0x9b, 0x88, 0x0e, 0x2d, 0x25, 0xef, 0x9c,
            0xe9, 0x9e, 0x77, 0x5c];

        assert!(keynonce.as_ref() == &expected_keynonce as &[u8]);
    }
}
