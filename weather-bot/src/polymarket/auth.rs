//! EIP-712 signing (ClobAuth + CTF Exchange orders) and CLOB L2 HMAC headers.
//! This is hand-rolled on k256/sha3. It has NOT been exercised against the live CLOB from the
//! build environment: verify it with a tiny order before trusting it (see README).
use crate::error::{BotError, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE};
use base64::Engine;
use hmac::{Hmac, Mac};
use k256::ecdsa::SigningKey;
use sha2::Sha256;
use sha3::{Digest, Keccak256};

pub fn keccak(data: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(data);
    h.finalize().into()
}

pub fn parse_addr(s: &str) -> Result<[u8; 20]> {
    let raw = hex::decode(s.trim().trim_start_matches("0x"))
        .map_err(|_| BotError::Config(format!("'{s}' is not a valid hex address")))?;
    if raw.len() != 20 {
        return Err(BotError::Config(format!("'{s}' is not a 20-byte address")));
    }
    let mut a = [0u8; 20];
    a.copy_from_slice(&raw);
    Ok(a)
}

pub fn checksum_address(a: &[u8; 20]) -> String {
    let lower = hex::encode(a);
    let h = keccak(lower.as_bytes());
    let mut out = String::from("0x");
    for (i, c) in lower.chars().enumerate() {
        let nib = if i % 2 == 0 { h[i / 2] >> 4 } else { h[i / 2] & 0x0f };
        if c.is_ascii_alphabetic() && nib >= 8 {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push(c);
        }
    }
    out
}

pub struct Wallet {
    key: SigningKey,
    addr: [u8; 20],
}

impl Wallet {
    pub fn from_hex(pk: &str) -> Result<Wallet> {
        let bytes = hex::decode(pk.trim().trim_start_matches("0x"))
            .map_err(|_| BotError::Config("POLYMARKET_PRIVATE_KEY is not valid hex".into()))?;
        let key = SigningKey::from_slice(&bytes)
            .map_err(|_| BotError::Config("POLYMARKET_PRIVATE_KEY is not a valid secp256k1 key".into()))?;
        let point = key.verifying_key().to_encoded_point(false);
        let h = keccak(&point.as_bytes()[1..]);
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&h[12..]);
        Ok(Wallet { key, addr })
    }

    pub fn address(&self) -> [u8; 20] {
        self.addr
    }

    pub fn address_hex(&self) -> String {
        checksum_address(&self.addr)
    }

    /// 65-byte r||s||v signature over a 32-byte digest, v in {27, 28}.
    pub fn sign_digest(&self, digest: &[u8; 32]) -> Result<String> {
        let (sig, rid) = self
            .key
            .sign_prehash_recoverable(digest)
            .map_err(|e| BotError::Execution(format!("signing failed: {e}")))?;
        let mut out = sig.to_bytes().to_vec();
        out.push(27 + rid.to_byte());
        Ok(format!("0x{}", hex::encode(out)))
    }
}

fn pad_addr(a: &[u8; 20]) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a);
    w
}

fn u64_word(n: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&n.to_be_bytes());
    w
}

fn u128_word(n: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&n.to_be_bytes());
    w
}

/// Decimal string (e.g. a Polymarket token id) to a 32-byte big-endian uint256.
pub fn dec_to_u256(s: &str) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    if s.is_empty() {
        return Err(BotError::Parse("empty uint256".into()));
    }
    for ch in s.bytes() {
        if !ch.is_ascii_digit() {
            return Err(BotError::Parse(format!("'{s}' is not a decimal integer")));
        }
        let mut carry = (ch - b'0') as u32;
        for i in (0..32).rev() {
            let v = out[i] as u32 * 10 + carry;
            out[i] = (v & 0xff) as u8;
            carry = v >> 8;
        }
        if carry != 0 {
            return Err(BotError::Parse("uint256 overflow".into()));
        }
    }
    Ok(out)
}

pub fn domain_separator(name: &str, version: &str, chain_id: u64, verifying: Option<[u8; 20]>) -> [u8; 32] {
    let ty = if verifying.is_some() {
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
    } else {
        "EIP712Domain(string name,string version,uint256 chainId)"
    };
    let mut buf = Vec::new();
    buf.extend(keccak(ty.as_bytes()));
    buf.extend(keccak(name.as_bytes()));
    buf.extend(keccak(version.as_bytes()));
    buf.extend(u64_word(chain_id));
    if let Some(v) = verifying {
        buf.extend(pad_addr(&v));
    }
    keccak(&buf)
}

pub fn eip712_digest(domain: [u8; 32], struct_hash: [u8; 32]) -> [u8; 32] {
    let mut b = Vec::with_capacity(66);
    b.extend([0x19u8, 0x01u8]);
    b.extend(domain);
    b.extend(struct_hash);
    keccak(&b)
}

pub struct ApiCreds {
    pub key: String,
    pub secret: String,
    pub passphrase: String,
}

/// L1 headers: wallet-signed ClobAuth message, used to create/derive API credentials.
pub fn l1_headers(wallet: &Wallet, chain_id: u64, ts: u64, nonce: u64) -> Result<Vec<(String, String)>> {
    let ty = keccak(b"ClobAuth(address address,string timestamp,uint256 nonce,string message)");
    let mut enc = Vec::new();
    enc.extend(ty);
    enc.extend(pad_addr(&wallet.address()));
    enc.extend(keccak(ts.to_string().as_bytes()));
    enc.extend(u64_word(nonce));
    enc.extend(keccak(b"This message attests that I control the given wallet"));
    let digest = eip712_digest(domain_separator("ClobAuthDomain", "1", chain_id, None), keccak(&enc));
    Ok(vec![
        ("POLY_ADDRESS".into(), wallet.address_hex()),
        ("POLY_SIGNATURE".into(), wallet.sign_digest(&digest)?),
        ("POLY_TIMESTAMP".into(), ts.to_string()),
        ("POLY_NONCE".into(), nonce.to_string()),
    ])
}

/// L2 headers: HMAC-SHA256 over timestamp+method+path+body with the base64url-decoded API secret.
pub fn l2_headers(
    creds: &ApiCreds,
    address_hex: &str,
    ts: u64,
    method: &str,
    path: &str,
    body: &str,
) -> Result<Vec<(String, String)>> {
    let secret = URL_SAFE
        .decode(creds.secret.trim())
        .or_else(|_| STANDARD.decode(creds.secret.trim()))
        .map_err(|_| BotError::Execution("API secret is not valid base64".into()))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&secret).map_err(|e| BotError::Execution(e.to_string()))?;
    mac.update(format!("{ts}{method}{path}{body}").as_bytes());
    let sig = URL_SAFE.encode(mac.finalize().into_bytes());
    Ok(vec![
        ("POLY_ADDRESS".into(), address_hex.to_string()),
        ("POLY_SIGNATURE".into(), sig),
        ("POLY_TIMESTAMP".into(), ts.to_string()),
        ("POLY_API_KEY".into(), creds.key.clone()),
        ("POLY_PASSPHRASE".into(), creds.passphrase.clone()),
    ])
}

pub struct OrderFields {
    pub salt: u64,
    pub maker: [u8; 20],
    pub signer: [u8; 20],
    pub taker: [u8; 20],
    pub token_id: String,
    pub maker_amount: u128,
    pub taker_amount: u128,
    pub expiration: u64,
    pub nonce: u64,
    pub fee_rate_bps: u64,
    /// 0 = BUY, 1 = SELL
    pub side: u8,
    pub signature_type: u8,
}

pub fn order_struct_hash(o: &OrderFields) -> Result<[u8; 32]> {
    let ty = keccak(
        b"Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)",
    );
    let mut enc = Vec::new();
    enc.extend(ty);
    enc.extend(u64_word(o.salt));
    enc.extend(pad_addr(&o.maker));
    enc.extend(pad_addr(&o.signer));
    enc.extend(pad_addr(&o.taker));
    enc.extend(dec_to_u256(&o.token_id)?);
    enc.extend(u128_word(o.maker_amount));
    enc.extend(u128_word(o.taker_amount));
    enc.extend(u64_word(o.expiration));
    enc.extend(u64_word(o.nonce));
    enc.extend(u64_word(o.fee_rate_bps));
    enc.extend(u64_word(o.side as u64));
    enc.extend(u64_word(o.signature_type as u64));
    Ok(keccak(&enc))
}

pub fn order_digest(o: &OrderFields, chain_id: u64, exchange: [u8; 20]) -> Result<[u8; 32]> {
    let dom = domain_separator("Polymarket CTF Exchange", "1", chain_id, Some(exchange));
    Ok(eip712_digest(dom, order_struct_hash(o)?))
}
