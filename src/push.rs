//! Web Push notification support via the [`web_push`] crate.
//!
//! VAPID keys are persisted to `~/.config/goop/vapid.toml` and
//! push subscriptions to `~/.config/goop/push_subscriptions.json`.

use std::path::PathBuf;
use std::sync::Mutex;

use base64::Engine;
use web_push::{
    ContentEncoding, IsahcWebPushClient, PartialVapidSignatureBuilder, SubscriptionInfo, Urgency,
    VapidSignatureBuilder, WebPushClient, WebPushMessageBuilder,
};

use crate::config;

// ── PushManager ───────────────────────────────────────────────────────

pub struct PushManager {
    /// Base64url-encoded 32-byte P-256 private key (no padding).
    vapid_private_b64: String,
    /// Uncompressed public key bytes (65 bytes, 0x04 prefix), base64url.
    vapid_public_b64: String,
    /// VAPID subject (e.g. "mailto:goop@localhost").
    subject: String,
    /// Active push subscriptions.
    subscriptions: Mutex<Vec<SubscriptionInfo>>,
    /// Persistence path for subscriptions.
    path: PathBuf,
}

// ── VAPID key persistence ────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize)]
struct VapidToml {
    private_key: String,
    subject: String,
}

fn vapid_path() -> PathBuf {
    config::config_dir().join("vapid.toml")
}

fn subs_path() -> PathBuf {
    config::config_dir().join("push_subscriptions.json")
}

fn encode_base64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// Load existing VAPID keys or generate a fresh pair.
/// Returns (private_key_base64url, public_key_base64url, subject).
fn load_or_generate_vapid() -> (String, String, String) {
    let path = vapid_path();

    // Try loading from disk.
    if let Ok(contents) = std::fs::read_to_string(&path)
        && let Ok(vapid) = toml::from_str::<VapidToml>(&contents)
    {
        // Reconstruct public key from private key via the web-push builder.
        if let Ok(builder) = VapidSignatureBuilder::from_base64_no_sub(&vapid.private_key) {
            let public_b64 = encode_base64url(&builder.get_public_key());
            return (vapid.private_key, public_b64, vapid.subject);
        }
    }

    // Generate fresh keys.
    let sk = p256::SecretKey::random(&mut rand::thread_rng());
    let private_b64 = encode_base64url(sk.to_bytes().as_slice());
    let pk = sk.public_key();
    let public_b64 = encode_base64url(pk.to_sec1_bytes().as_ref());
    let subject = "mailto:goop@localhost".to_string();

    let vapid = VapidToml {
        private_key: private_b64.clone(),
        subject: subject.clone(),
    };

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(contents) = toml::to_string_pretty(&vapid) {
        let _ = std::fs::write(&path, contents);
        tracing::info!("generated new VAPID key pair at {}", path.display());
    }

    (private_b64, public_b64, subject)
}

// ── PushManager impl ──────────────────────────────────────────────────

impl PushManager {
    pub fn new() -> Self {
        let (vapid_private_b64, vapid_public_b64, subject) = load_or_generate_vapid();
        let path = subs_path();

        let subscriptions = if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str::<Vec<SubscriptionInfo>>(&s).ok())
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        tracing::info!(
            "push manager ready — {} subscription(s)",
            subscriptions.len()
        );

        Self {
            vapid_private_b64,
            vapid_public_b64,
            subject,
            subscriptions: Mutex::new(subscriptions),
            path,
        }
    }

    /// Return the VAPID public key (uncompressed, base64url) for the
    /// client to use as `applicationServerKey`.
    pub fn vapid_public_key_b64(&self) -> &str {
        &self.vapid_public_b64
    }

    /// Add or refresh a push subscription. Deduplicates by endpoint.
    pub fn add_subscription(&self, sub: SubscriptionInfo) {
        let mut subs = self.subscriptions.lock().unwrap();
        subs.retain(|s| s.endpoint != sub.endpoint);
        subs.push(sub);
        self.save(&subs);
    }

    /// Fire a push notification to all registered subscriptions.
    /// Spawned in a background task by the session — never blocks.
    pub async fn notify(&self, session_name: &str, event: &str) {
        let payload = serde_json::json!({
            "session": session_name,
            "event": event,
        })
        .to_string();

        let subs = {
            let subs = self.subscriptions.lock().unwrap();
            subs.clone()
        };

        if subs.is_empty() {
            return;
        }

        // Build the VAPID signer once — it's cheap to clone per subscription.
        let Ok(sig_builder) = VapidSignatureBuilder::from_base64_no_sub(&self.vapid_private_b64)
        else {
            tracing::error!("invalid VAPID private key — cannot sign push messages");
            return;
        };

        let client = match IsahcWebPushClient::new() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to create push client: {e}");
                return;
            }
        };

        let mut removed = Vec::new();

        for sub in &subs {
            match send_push(&client, &sig_builder, sub, &payload, &self.subject).await {
                Ok(()) => {
                    tracing::debug!(
                        "push sent to {}…",
                        &sub.endpoint[..60.min(sub.endpoint.len())]
                    );
                }
                Err(PushError::Gone) => {
                    tracing::info!("push subscription expired — removing");
                    removed.push(sub.endpoint.clone());
                }
                Err(e) => {
                    tracing::warn!("push failed: {e}");
                }
            }
        }

        if !removed.is_empty() {
            let mut subs = self.subscriptions.lock().unwrap();
            subs.retain(|s| !removed.contains(&s.endpoint));
            self.save(&subs);
        }
    }

    fn save(&self, subs: &[SubscriptionInfo]) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(subs) {
            let _ = std::fs::write(&self.path, json);
        }
    }
}

// ── push sending ─────────────────────────────────────────────────────

#[derive(Debug)]
enum PushError {
    Gone,
    Other(anyhow::Error),
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushError::Gone => write!(f, "subscription gone (410)"),
            PushError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl From<anyhow::Error> for PushError {
    fn from(e: anyhow::Error) -> Self {
        PushError::Other(e)
    }
}

impl From<web_push::WebPushError> for PushError {
    fn from(e: web_push::WebPushError) -> Self {
        PushError::Other(anyhow::anyhow!("{}", e))
    }
}

/// Send a single push message using the web-push crate.
async fn send_push(
    client: &IsahcWebPushClient,
    sig_builder: &PartialVapidSignatureBuilder,
    sub: &SubscriptionInfo,
    payload: &str,
    subject: &str,
) -> Result<(), PushError> {
    // Build VAPID signature for this subscription.
    let mut sig = sig_builder.clone().add_sub_info(sub);
    sig.add_claim("sub", subject);
    let sig = sig.build()?;

    let mut builder = WebPushMessageBuilder::new(sub);
    builder.set_payload(ContentEncoding::Aes128Gcm, payload.as_bytes());
    builder.set_vapid_signature(sig);
    builder.set_urgency(Urgency::Normal);
    builder.set_ttl(86400);

    let message = builder.build()?;
    client.send(message).await.map_err(|e| {
        if matches!(e, web_push::WebPushError::EndpointNotValid(_)) {
            PushError::Gone
        } else {
            PushError::Other(anyhow::anyhow!("{}", e))
        }
    })
}
