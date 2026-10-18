use super::order_builder::{OrderRequest, OrderType};
use super::{Broker, OrderAck, OrderState, OrderStatus};
use crate::config::EndpointCfg;
use crate::error::{BotError, Result};
use crate::polymarket::auth::{self, parse_addr, ApiCreds, OrderFields, Wallet};
use crate::polymarket::clob::ClobClient;
use crate::util::math::{json_dec, parse_dec};
use crate::util::time::retry;
use async_trait::async_trait;
use rand::Rng;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::sync::Arc;

/// Live CLOB broker. Order signing (EIP-712) and L1/L2 auth are implemented in polymarket/auth.rs and
/// have not been exercised against the live exchange from the build environment: smoke-test with a
/// tiny size before running real size.
pub struct LiveBroker {
    http: reqwest::Client,
    base: String,
    wallet: Wallet,
    funder: [u8; 20],
    signature_type: u8,
    chain_id: u64,
    exchange: [u8; 20],
    neg_risk_exchange: [u8; 20],
    creds: ApiCreds,
    clob: Arc<ClobClient>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl LiveBroker {
    pub async fn connect(
        http: reqwest::Client,
        ep: &EndpointCfg,
        wallet: Wallet,
        funder: [u8; 20],
        signature_type: u8,
        clob: Arc<ClobClient>,
    ) -> Result<LiveBroker> {
        let base = ep.clob_url.trim_end_matches('/').to_string();
        let creds = Self::derive_creds(&http, &base, &wallet, ep.chain_id).await?;
        Ok(LiveBroker {
            http,
            base,
            wallet,
            funder,
            signature_type,
            chain_id: ep.chain_id,
            exchange: parse_addr(&ep.exchange)?,
            neg_risk_exchange: parse_addr(&ep.neg_risk_exchange)?,
            creds,
            clob,
        })
    }

    async fn derive_creds(http: &reqwest::Client, base: &str, wallet: &Wallet, chain_id: u64) -> Result<ApiCreds> {
        for (method, path) in [("GET", "/auth/derive-api-key"), ("POST", "/auth/api-key")] {
            let headers = auth::l1_headers(wallet, chain_id, unix_now(), 0)?;
            let mut rb = if method == "GET" { http.get(format!("{base}{path}")) } else { http.post(format!("{base}{path}")) };
            for (k, v) in headers {
                rb = rb.header(k, v);
            }
            let resp = match tokio::time::timeout(std::time::Duration::from_secs(5), rb.send()).await {
                Ok(Ok(r)) => r,
                _ => continue,
            };
            if !resp.status().is_success() {
                continue;
            }
            let v: serde_json::Value = resp.json().await.map_err(|e| BotError::Network(e.to_string()))?;
            let g = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string());
            if let (Some(key), Some(secret), Some(passphrase)) = (g("apiKey"), g("secret"), g("passphrase")) {
                return Ok(ApiCreds { key, secret, passphrase });
            }
        }
        Err(BotError::Execution("could not create or derive CLOB API credentials".into()))
    }

    async fn signed(&self, method: &str, path: &str, body: Option<String>) -> Result<serde_json::Value> {
        let http = self.http.clone();
        let url = format!("{}{}", self.base, path);
        let method = method.to_string();
        let path = path.to_string();
        let body = body.unwrap_or_default();
        let addr = self.wallet.address_hex();
        let creds = ApiCreds {
            key: self.creds.key.clone(),
            secret: self.creds.secret.clone(),
            passphrase: self.creds.passphrase.clone(),
        };
        retry("clob-signed", move || {
            let (http, url, method, path, body, addr) =
                (http.clone(), url.clone(), method.clone(), path.clone(), body.clone(), addr.clone());
            let creds = ApiCreds { key: creds.key.clone(), secret: creds.secret.clone(), passphrase: creds.passphrase.clone() };
            async move {
                let headers = auth::l2_headers(&creds, &addr, unix_now(), &method, &path, &body)?;
                let mut rb = match method.as_str() {
                    "GET" => http.get(&url),
                    "POST" => http.post(&url),
                    _ => http.delete(&url),
                };
                for (k, v) in headers {
                    rb = rb.header(k, v);
                }
                if !body.is_empty() {
                    rb = rb.header("Content-Type", "application/json").body(body.clone());
                }
                let resp = rb.send().await.map_err(|e| BotError::Network(e.to_string()))?;
                let status = resp.status();
                let text = resp.text().await.map_err(|e| BotError::Network(e.to_string()))?;
                if !status.is_success() {
                    return Err(BotError::Execution(format!("{method} {path}: HTTP {status}: {text}")));
                }
                serde_json::from_str(&text).map_err(|e| BotError::Parse(format!("{path}: {e}")))
            }
        })
        .await
    }

    fn scale6(x: Decimal) -> Result<u128> {
        (x * Decimal::new(1_000_000, 0))
            .floor()
            .to_u128()
            .ok_or_else(|| BotError::Execution("amount out of range".into()))
    }
}

#[async_trait]
impl Broker for LiveBroker {
    async fn place(&self, req: &OrderRequest) -> Result<OrderAck> {
        let fee_bps = self.clob.fee_rate_bps(&req.token_id).await?;
        // BUY: we give USDC (makerAmount), we receive shares (takerAmount); both in 1e-6 units.
        let maker_amount = Self::scale6(req.price * req.size)?;
        let taker_amount = Self::scale6(req.size)?;
        let salt: u64 = rand::thread_rng().gen::<u64>() & ((1u64 << 53) - 1);
        let fields = OrderFields {
            salt,
            maker: self.funder,
            signer: self.wallet.address(),
            taker: [0u8; 20],
            token_id: req.token_id.clone(),
            maker_amount,
            taker_amount,
            expiration: 0,
            nonce: 0,
            fee_rate_bps: fee_bps,
            side: 0,
            signature_type: self.signature_type,
        };
        let exchange = if req.neg_risk { self.neg_risk_exchange } else { self.exchange };
        let digest = auth::order_digest(&fields, self.chain_id, exchange)?;
        let signature = self.wallet.sign_digest(&digest)?;
        let order_type = match req.order_type {
            OrderType::Gtc => "GTC",
            OrderType::Fak => "FAK",
        };
        let body = serde_json::json!({
            "order": {
                "salt": salt,
                "maker": auth::checksum_address(&self.funder),
                "signer": self.wallet.address_hex(),
                "taker": "0x0000000000000000000000000000000000000000",
                "tokenId": req.token_id,
                "makerAmount": maker_amount.to_string(),
                "takerAmount": taker_amount.to_string(),
                "expiration": "0",
                "nonce": "0",
                "feeRateBps": fee_bps.to_string(),
                "side": "BUY",
                "signatureType": self.signature_type,
                "signature": signature,
            },
            "owner": self.creds.key,
            "orderType": order_type,
            "postOnly": req.post_only,
        })
        .to_string();
        let v = self.signed("POST", "/order", Some(body)).await?;
        let ok = v.get("success").and_then(|x| x.as_bool()).unwrap_or(false);
        let id = v.get("orderID").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if !ok || id.is_empty() {
            let msg = v.get("errorMsg").and_then(|x| x.as_str()).unwrap_or("order rejected").to_string();
            return Ok(OrderAck { id, state: OrderState::Rejected(msg), filled: Decimal::ZERO, avg_price: Decimal::ZERO });
        }
        // For a BUY, takingAmount = shares received and makingAmount = USDC paid.
        let shares = v.get("takingAmount").and_then(json_dec).unwrap_or(Decimal::ZERO);
        let usdc = v.get("makingAmount").and_then(json_dec).unwrap_or(Decimal::ZERO);
        let avg = if shares > Decimal::ZERO && usdc > Decimal::ZERO { usdc / shares } else { req.price };
        let state = if shares >= req.size { OrderState::Filled } else { OrderState::Open };
        Ok(OrderAck { id, state, filled: shares, avg_price: avg })
    }

    async fn cancel(&self, order_id: &str) -> Result<()> {
        let body = serde_json::json!({ "orderID": order_id }).to_string();
        self.signed("DELETE", "/order", Some(body)).await.map(|_| ())
    }

    async fn status(&self, order_id: &str) -> Result<OrderStatus> {
        let v = self.signed("GET", &format!("/data/order/{order_id}"), None).await?;
        let status = v.get("status").and_then(|x| x.as_str()).unwrap_or("").to_uppercase();
        let matched = v.get("size_matched").and_then(json_dec).unwrap_or(Decimal::ZERO);
        let original = v.get("original_size").and_then(json_dec).unwrap_or(Decimal::ZERO);
        let price = v.get("price").and_then(|x| x.as_str()).and_then(parse_dec).unwrap_or(Decimal::ZERO);
        let state = if matched > Decimal::ZERO && matched >= original {
            OrderState::Filled
        } else if status.contains("CANCEL") {
            OrderState::Cancelled
        } else if status.contains("INVALID") {
            OrderState::Rejected(status.clone())
        } else {
            OrderState::Open
        };
        Ok(OrderStatus { state, filled: matched, avg_price: price })
    }

    fn name(&self) -> &'static str {
        "live"
    }
}
