//! Operator-defined "custom HTTP gateway": configured entirely from the admin
//! (rows with id `custom-<slug>`), no code needed.
//!
//! * `create`: templated request; the response gives the redirect URL and the
//!   provider reference via dotted JSON paths.
//! * `webhook`: HMAC-SHA256 signature check (hex or base64, optional prefix,
//!   optional timestamp header + tolerance) and dotted paths / event-type
//!   lists that map events to payment states.
//! * `refund` (optional): templated request plus status mapping.
//!
//! Template variables: `{{amount}}` (decimal), `{{amount_minor}}`,
//! `{{currency}}`, `{{payment_id}}`, `{{reference}}`, `{{description}}`,
//! `{{email}}`, `{{success_url}}`, `{{cancel_url}}`, `{{webhook_url}}`,
//! `{{idempotency_key}}`, `{{provider_ref}}` (refunds) and `{{secret.NAME}}`.
//! Values are JSON-escaped inside JSON bodies, URL-encoded inside URLs and
//! form bodies. Only HTTPS endpoints are allowed (http on loopback for tests).
use base64::Engine;
use pelty_payments::http::{api_base, ct_eq, hmac_sha256, send_json, url_encode};
use pelty_payments::*;
#[allow(unused_imports)]
use {
    pelty_core::money::Money,
    serde::{Deserialize, Serialize},
    std::collections::{BTreeMap, HashMap},
    std::future::Future,
    std::pin::Pin,
    std::sync::Arc,
};

pub const ID: &str = "custom_http";

#[derive(Debug, Clone, Deserialize)]
pub struct RequestTemplate {
    #[serde(default = "post")]
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: String,
    /// `json` (default) or `form`.
    #[serde(default = "json_kind")]
    pub body_format: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateCfg {
    #[serde(flatten)]
    pub request: RequestTemplate,
    pub redirect_url_path: String,
    pub ref_path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebhookCfg {
    pub signature_header: String,
    /// Secret name holding the HMAC key.
    #[serde(default = "wh_secret")]
    pub secret: String,
    /// `hex` (default) or `base64`.
    #[serde(default = "hex_enc")]
    pub encoding: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub timestamp_header: Option<String>,
    /// `{body}` (default) or e.g. `{timestamp}.{body}`.
    #[serde(default = "body_tpl")]
    pub signed_payload: String,
    #[serde(default = "tolerance")]
    pub tolerance_secs: i64,
    pub event_id_path: String,
    pub event_type_path: String,
    pub ref_path: String,
    #[serde(default)]
    pub payment_id_path: Option<String>,
    #[serde(default)]
    pub success_types: Vec<String>,
    #[serde(default)]
    pub failed_types: Vec<String>,
    #[serde(default)]
    pub refunded_types: Vec<String>,
    #[serde(default)]
    pub disputed_types: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RefundCfg {
    #[serde(flatten)]
    pub request: RequestTemplate,
    #[serde(default)]
    pub ref_path: Option<String>,
    #[serde(default)]
    pub status_path: Option<String>,
    #[serde(default)]
    pub success_values: Vec<String>,
    #[serde(default)]
    pub failed_values: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CustomCfg {
    #[serde(default)]
    pub name: Option<String>,
    pub create: CreateCfg,
    pub webhook: Option<WebhookCfg>,
    pub refund: Option<RefundCfg>,
}

fn post() -> String {
    "POST".into()
}
fn json_kind() -> String {
    "json".into()
}
fn wh_secret() -> String {
    "webhook_secret".into()
}
fn hex_enc() -> String {
    "hex".into()
}
fn body_tpl() -> String {
    "{body}".into()
}
fn tolerance() -> i64 {
    300
}

const SETTINGS: &[SettingField] = &[
    SettingField::text("name", true),
    SettingField::of("create", "json"),
    SettingField::of("webhook", "json"),
    SettingField::of("refund", "json"),
];

pub fn descriptor() -> GatewayDescriptor {
    GatewayDescriptor {
        id: ID,
        name: "Custom HTTP gateway",
        capabilities: Capabilities {
            checkout: true,
            webhooks: true,
            refunds: true,
            recurring: false,
        },
        secrets: &[],
        settings: SETTINGS,
        currencies: &[],
    }
}

/// Parse + validate an admin-supplied config (also used by the API on save).
pub fn parse(config: &serde_json::Value) -> Result<CustomCfg, GatewayError> {
    let c: CustomCfg =
        serde_json::from_value(config.clone()).map_err(|e| GatewayError::Config(e.to_string()))?;
    let check = |r: &RequestTemplate| -> Result<(), GatewayError> {
        if !matches!(r.method.as_str(), "GET" | "POST" | "PUT" | "PATCH") {
            return Err(GatewayError::Config("method".into()));
        }
        if !matches!(r.body_format.as_str(), "json" | "form") {
            return Err(GatewayError::Config("body_format".into()));
        }
        // Validate the static scheme/host part (before any template).
        let host_part = r.url.split("{{").next().unwrap_or_default();
        api_base(&serde_json::json!({"api_base": host_part}), "")?;
        if host_part.is_empty() {
            return Err(GatewayError::Config("url".into()));
        }
        Ok(())
    };
    check(&c.create.request)?;
    if let Some(r) = &c.refund {
        check(&r.request)?;
    }
    if let Some(w) = &c.webhook {
        if !matches!(w.encoding.as_str(), "hex" | "base64") || w.signature_header.trim().is_empty()
        {
            return Err(GatewayError::Config("webhook signature".into()));
        }
        if w.signed_payload.contains("{timestamp}") && w.timestamp_header.is_none() {
            return Err(GatewayError::Config("timestamp_header".into()));
        }
    }
    Ok(c)
}

pub struct CustomGateway {
    cfg: CustomCfg,
    secrets: HashMap<String, String>,
    t: Arc<dyn HttpTransport>,
}

pub fn factory(cfg: &GatewayConfig) -> Result<Box<dyn PaymentGateway>, GatewayError> {
    Ok(Box::new(CustomGateway {
        cfg: parse(&cfg.config)?,
        secrets: cfg.secrets.clone(),
        t: cfg.transport.clone(),
    }))
}

fn json_escape(s: &str) -> String {
    let q = serde_json::Value::String(s.to_string()).to_string();
    q[1..q.len() - 1].to_string()
}

/// Replace `{{var}}` placeholders; unknown variables are an error.
pub fn render(
    tpl: &str,
    vars: &HashMap<String, String>,
    enc: fn(&str) -> String,
) -> Result<String, GatewayError> {
    let mut out = String::with_capacity(tpl.len());
    let mut rest = tpl;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let j = after
            .find("}}")
            .ok_or_else(|| GatewayError::Config("unclosed {{".into()))?;
        let key = after[..j].trim();
        let v = vars
            .get(key)
            .ok_or_else(|| GatewayError::Config(format!("unknown template variable {key}")))?;
        out.push_str(&enc(v));
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn ident(s: &str) -> String {
    s.to_string()
}

impl CustomGateway {
    fn vars(&self) -> HashMap<String, String> {
        self.secrets
            .iter()
            .map(|(k, v)| (format!("secret.{k}"), v.clone()))
            .collect()
    }
    fn build(
        &self,
        r: &RequestTemplate,
        vars: &HashMap<String, String>,
    ) -> Result<HttpRequestSpec, GatewayError> {
        let url = render(&r.url, vars, url_encode)?;
        api_base(&serde_json::json!({"api_base": url}), "")?;
        let (body, ct) = if r.body_format == "form" {
            (
                render(&r.body, vars, url_encode)?,
                "application/x-www-form-urlencoded",
            )
        } else {
            let b = render(&r.body, vars, json_escape)?;
            if !b.is_empty() {
                serde_json::from_str::<serde_json::Value>(&b)
                    .map_err(|_| GatewayError::Config("body template is not valid JSON".into()))?;
            }
            (b, "application/json")
        };
        let mut headers = vec![];
        for (k, v) in &r.headers {
            headers.push((k.clone(), render(v, vars, ident)?));
        }
        if !body.is_empty()
            && !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        {
            headers.push(("Content-Type".into(), ct.into()));
        }
        let method = match r.method.as_str() {
            "GET" => "GET",
            "PUT" => "PUT",
            "PATCH" => "PATCH",
            _ => "POST",
        };
        Ok(HttpRequestSpec {
            method,
            url,
            headers,
            body,
        })
    }
}

impl PaymentGateway for CustomGateway {
    fn id(&self) -> &'static str {
        ID
    }
    fn display_name(&self) -> &'static str {
        "Custom HTTP gateway"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            checkout: true,
            webhooks: self.cfg.webhook.is_some(),
            refunds: self.cfg.refund.is_some(),
            recurring: false,
        }
    }
    fn create_payment<'a>(
        &'a self,
        r: &'a PaymentRequest,
    ) -> BoxFuture<'a, Result<PaymentInit, GatewayError>> {
        Box::pin(async move {
            let mut v = self.vars();
            for (k, val) in [
                ("amount", decimal_str(r.amount)?),
                ("amount_minor", minor_units(r.amount)?.to_string()),
                ("currency", r.amount.currency.as_str().to_string()),
                ("payment_id", r.payment_id.clone()),
                ("reference", r.reference.clone()),
                ("description", r.description.clone()),
                ("email", r.customer_email.clone().unwrap_or_default()),
                ("success_url", r.success_url.clone()),
                ("cancel_url", r.cancel_url.clone()),
                ("webhook_url", r.webhook_url.clone()),
                ("idempotency_key", r.idempotency_key.clone()),
            ] {
                v.insert(k.into(), val);
            }
            let spec = self.build(&self.cfg.create.request, &v)?;
            let res = send_json(self.t.as_ref(), spec).await?;
            match (
                obj_str(&res, &self.cfg.create.redirect_url_path),
                obj_str(&res, &self.cfg.create.ref_path),
            ) {
                (Some(url), Some(r))
                    if url.starts_with("https://") || url.starts_with("http://127.0.0.1") =>
                {
                    Ok(PaymentInit::Redirect {
                        url,
                        provider_ref: r,
                    })
                }
                _ => Err(GatewayError::Provider(
                    "response lacks redirect url/ref".into(),
                )),
            }
        })
    }
    fn parse_webhook(
        &self,
        h: &Headers,
        body: &[u8],
        now: i64,
    ) -> Result<WebhookEvent, GatewayError> {
        let w = self.cfg.webhook.as_ref().ok_or(GatewayError::Unsupported)?;
        let key = self
            .secrets
            .get(&w.secret)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| GatewayError::Config(format!("missing secret {}", w.secret)))?;
        let raw = header(h, &w.signature_header.to_ascii_lowercase())
            .ok_or(GatewayError::InvalidSignature)?;
        let sig = raw
            .trim()
            .strip_prefix(w.prefix.as_str())
            .ok_or(GatewayError::InvalidSignature)?;
        let got = if w.encoding == "base64" {
            base64::engine::general_purpose::STANDARD
                .decode(sig)
                .map_err(|_| GatewayError::InvalidSignature)?
        } else {
            hex::decode(sig).map_err(|_| GatewayError::InvalidSignature)?
        };
        let body_s = std::str::from_utf8(body).map_err(|_| GatewayError::MalformedPayload)?;
        let mut payload = w.signed_payload.clone();
        if let Some(th) = &w.timestamp_header {
            let ts = header(h, &th.to_ascii_lowercase()).ok_or(GatewayError::InvalidSignature)?;
            let n: i64 = ts
                .trim()
                .parse()
                .map_err(|_| GatewayError::InvalidSignature)?;
            if (now - n).abs() > w.tolerance_secs {
                return Err(GatewayError::InvalidSignature);
            }
            payload = payload.replace("{timestamp}", ts.trim());
        }
        let payload = payload.replace("{body}", body_s);
        if !ct_eq(&got, &hmac_sha256(key.as_bytes(), payload.as_bytes())) {
            return Err(GatewayError::InvalidSignature);
        }
        let v: serde_json::Value =
            serde_json::from_slice(body).map_err(|_| GatewayError::MalformedPayload)?;
        let id = obj_str(&v, &w.event_id_path).ok_or(GatewayError::MalformedPayload)?;
        let ty = obj_str(&v, &w.event_type_path).ok_or(GatewayError::MalformedPayload)?;
        let has = |l: &Vec<String>| l.iter().any(|x| x == &ty);
        let kind = if has(&w.success_types) {
            WebhookKind::PaymentSucceeded
        } else if has(&w.failed_types) {
            WebhookKind::PaymentFailed
        } else if has(&w.refunded_types) {
            WebhookKind::Refunded
        } else if has(&w.disputed_types) {
            WebhookKind::Disputed
        } else {
            WebhookKind::Ignored
        };
        let mut e = WebhookEvent::new(id, ty, kind, obj_str(&v, &w.ref_path));
        e.payment_id = w.payment_id_path.as_deref().and_then(|p| obj_str(&v, p));
        Ok(e)
    }
    fn refund<'a>(
        &'a self,
        provider_ref: &'a str,
        amount: Money,
        idem: &'a str,
    ) -> BoxFuture<'a, Result<RefundResult, GatewayError>> {
        Box::pin(async move {
            let r = self.cfg.refund.as_ref().ok_or(GatewayError::Unsupported)?;
            let mut v = self.vars();
            v.insert("provider_ref".into(), provider_ref.into());
            v.insert("amount".into(), decimal_str(amount)?);
            v.insert("amount_minor".into(), minor_units(amount)?.to_string());
            v.insert("currency".into(), amount.currency.as_str().into());
            v.insert("idempotency_key".into(), idem.into());
            let res = send_json(self.t.as_ref(), self.build(&r.request, &v)?).await?;
            let st = r.status_path.as_deref().and_then(|p| obj_str(&res, p));
            let status = match st {
                None => "succeeded",
                Some(s) if r.success_values.contains(&s) => "succeeded",
                Some(s) if r.failed_values.contains(&s) => "failed",
                Some(_) => "pending",
            };
            Ok(RefundResult {
                provider_ref: r.ref_path.as_deref().and_then(|p| obj_str(&res, p)),
                status: status.into(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pelty_payments::test_util::*;

    fn conf() -> serde_json::Value {
        serde_json::json!({
            "name": "Acme Pay",
            "create": {
                "url": "https://api.acme.example/v1/pay?ref={{reference}}",
                "headers": {"Authorization": "Bearer {{secret.api_key}}"},
                "body": "{\"amount\":\"{{amount}}\",\"currency\":\"{{currency}}\",\"desc\":\"{{description}}\",\"order\":\"{{payment_id}}\"}",
                "redirect_url_path": "data.checkout_url",
                "ref_path": "data.id"
            },
            "webhook": {
                "signature_header": "X-Acme-Signature", "prefix": "sha256=",
                "timestamp_header": "X-Acme-Timestamp", "signed_payload": "{timestamp}.{body}",
                "event_id_path": "id", "event_type_path": "type", "ref_path": "data.id",
                "success_types": ["payment.paid"], "failed_types": ["payment.failed"]
            },
            "refund": {
                "url": "https://api.acme.example/v1/pay/{{provider_ref}}/refund",
                "body": "{\"amount_minor\":{{amount_minor}}}",
                "ref_path": "id", "status_path": "state", "success_values": ["done"]
            }
        })
    }

    #[tokio::test]
    async fn templated_create_webhook_refund() {
        let t = MockTransport::new();
        let mut reg = GatewayRegistry::with_transport(Arc::new(t.clone()));
        reg.register(descriptor(), factory);
        let c = GatewayConfig {
            config: conf(),
            secrets: [
                ("api_key".to_string(), "k1".to_string()),
                ("webhook_secret".to_string(), "whs".to_string()),
            ]
            .into(),
            ..Default::default()
        };
        let gw = reg.build("custom-acme", &c).unwrap();
        t.push(
            200,
            r#"{"data":{"id":"A1","checkout_url":"https://pay.acme.example/A1"}}"#,
        );
        let mut rq = req("7", "EUR");
        rq.description = "VPS \"S\" & more".into();
        rq.reference = "INV 7/a".into();
        let init = gw.create_payment(&rq).await.unwrap();
        assert_eq!(
            init,
            PaymentInit::Redirect {
                url: "https://pay.acme.example/A1".into(),
                provider_ref: "A1".into()
            }
        );
        let s = &t.requests()[0];
        assert_eq!(s.url, "https://api.acme.example/v1/pay?ref=INV+7%2Fa");
        assert_eq!(s.header("authorization"), Some("Bearer k1"));
        let b: serde_json::Value = serde_json::from_str(&s.body).unwrap();
        assert_eq!(
            (b["amount"].as_str(), b["desc"].as_str()),
            (Some("7.00"), Some("VPS \"S\" & more"))
        );

        let body = r#"{"id":"ev1","type":"payment.paid","data":{"id":"A1"}}"#;
        // python3: hmac.new(b"whs", b"1700000000."+body, sha256).hexdigest()
        let real = "64f0f7aef9024e9f92d213a7d9221cb74403c245b556f8f5423c1dd150b1500b";
        let h = hdrs(&[
            ("x-acme-signature", &format!("sha256={real}")),
            ("x-acme-timestamp", "1700000000"),
        ]);
        let e = gw
            .parse_webhook(&h, body.as_bytes(), 1_700_000_001)
            .unwrap();
        assert_eq!(
            (e.kind, e.provider_ref.as_deref()),
            (WebhookKind::PaymentSucceeded, Some("A1"))
        );
        assert!(
            gw.parse_webhook(&h, body.as_bytes(), 1_700_001_000)
                .is_err(),
            "stale"
        );
        let bad = hdrs(&[
            ("x-acme-signature", real),
            ("x-acme-timestamp", "1700000000"),
        ]);
        assert!(
            gw.parse_webhook(&bad, body.as_bytes(), 1_700_000_001)
                .is_err(),
            "prefix required"
        );

        t.push(200, r#"{"id":"R1","state":"done"}"#);
        let r = gw.refund("A1", money("2", "EUR"), "k").await.unwrap();
        assert_eq!(
            r,
            RefundResult {
                provider_ref: Some("R1".into()),
                status: "succeeded".into()
            }
        );
        let last = t.requests().last().unwrap().clone();
        assert_eq!(
            (last.url.as_str(), last.body.as_str()),
            (
                "https://api.acme.example/v1/pay/A1/refund",
                r#"{"amount_minor":200}"#
            )
        );
    }

    #[test]
    fn validation_rejects_unsafe_configs() {
        let mut c = conf();
        c["create"]["url"] = "http://evil.example/pay".into();
        assert!(parse(&c).is_err(), "plain http");
        let mut c = conf();
        c["create"]["body"] = "{{nope}}".into();
        let g = CustomGateway {
            cfg: parse(&c).unwrap(),
            secrets: HashMap::new(),
            t: Arc::new(NoTransport),
        };
        assert!(
            g.build(&g.cfg.create.request, &HashMap::new()).is_err(),
            "unknown var"
        );
        let mut c = conf();
        c["webhook"]["timestamp_header"] = serde_json::Value::Null;
        assert!(parse(&c).is_err(), "timestamp payload without header");
        assert!(parse(&serde_json::json!({})).is_err());
    }
}
