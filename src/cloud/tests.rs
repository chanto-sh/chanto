//! Cloud credential and wire-format tests. Every test builds its own [`Vars`]
//! over a temp directory: nothing reads the real home or calls a cloud.

use super::*;
use crate::llm::mock::serve;
use chrono::TimeZone;

fn tmp() -> PathBuf {
    let d = std::env::temp_dir().join(format!("chanto-cloud-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write(p: &std::path::Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// An executable script printing `out` and logging its arguments to `<path>.runs`.
#[cfg(unix)]
fn script(path: &std::path::Path, out: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let runs = path.with_extension("runs");
    write(
        path,
        &format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\ncat <<'EOF'\n{out}\nEOF\n",
            runs.display()
        ),
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

#[cfg(unix)]
fn runs(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path.with_extension("runs"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------- SigV4

const AKID: &str = "AKIDEXAMPLE";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

fn suite(method: &str, url: &str, headers: &[(&str, &str)], body: &str) -> String {
    let creds = sigv4::Credentials {
        access_key_id: AKID.into(),
        secret_access_key: SECRET.into(),
        session_token: None,
    };
    let t = chrono::Utc
        .with_ymd_and_hms(2015, 8, 30, 12, 36, 0)
        .unwrap();
    let h = sigv4::sign(
        method,
        &reqwest::Url::parse(url).unwrap(),
        headers,
        body.as_bytes(),
        &creds,
        "us-east-1",
        "service",
        t,
    );
    assert_eq!(
        h[0],
        ("x-amz-date".to_string(), "20150830T123600Z".to_string())
    );
    h.into_iter().find(|(k, _)| k == "authorization").unwrap().1
}

#[test]
fn sigv4_matches_the_aws_test_suite() {
    // aws-sig-v4-test-suite: get-vanilla, post-vanilla, get-vanilla-query-order-key-case, post-x-www-form-urlencoded.
    let cred = "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request";
    assert_eq!(
        suite("GET", "https://example.amazonaws.com/", &[], ""),
        format!(
            "{cred}, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        )
    );
    assert_eq!(
        suite("POST", "https://example.amazonaws.com/", &[], ""),
        format!(
            "{cred}, SignedHeaders=host;x-amz-date, Signature=5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b"
        )
    );
    assert_eq!(
        suite(
            "GET",
            "https://example.amazonaws.com/?Param2=value2&Param1=value1",
            &[],
            ""
        ),
        format!(
            "{cred}, SignedHeaders=host;x-amz-date, Signature=b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"
        )
    );
    assert_eq!(
        suite(
            "POST",
            "https://example.amazonaws.com/",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            "Param1=value1"
        ),
        format!(
            "{cred}, SignedHeaders=content-type;host;x-amz-date, Signature=ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a"
        )
    );
    // The derived-key example from the SigV4 documentation.
    assert_eq!(
        sigv4::hex(&sigv4::signing_key(SECRET, "20120215", "us-east-1", "iam")),
        "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
    );
}

#[test]
fn sigv4_double_encodes_bedrock_model_ids_and_signs_the_session_token() {
    // Expected value from an independent implementation (Python hmac/hashlib).
    let creds = sigv4::Credentials {
        access_key_id: "AKID".into(),
        secret_access_key: "SECRET".into(),
        session_token: Some("tok/en==".into()),
    };
    let url = format!(
        "https://bedrock-runtime.us-west-2.amazonaws.com/model/{}/invoke",
        urlencoding::encode("us.anthropic.claude-x-v1:0")
    );
    assert!(url.ends_with("claude-x-v1%3A0/invoke"));
    let t = chrono::Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let h = sigv4::sign(
        "POST",
        &reqwest::Url::parse(&url).unwrap(),
        &[("content-type", "application/json")],
        br#"{"a":1}"#,
        &creds,
        "us-west-2",
        "bedrock",
        t,
    );
    assert_eq!(
        h[1],
        ("x-amz-security-token".to_string(), "tok/en==".to_string())
    );
    assert_eq!(
        h[2].1,
        "AWS4-HMAC-SHA256 Credential=AKID/20240101/us-west-2/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token, Signature=d9db686008e0e27b0c902d5dad6619fb50526251d52cd326748d82d28c9bfdb4"
    );
}

// ---------------------------------------------------------------- event stream

#[test]
fn event_stream_frames_split_joined_and_corrupt() {
    let a = eventstream::encode(
        &[(":message-type", "event"), (":event-type", "chunk")],
        br#"{"bytes":"eA=="}"#,
    );
    let b = eventstream::encode(&[(":event-type", "metadata")], b"{}");
    // Byte by byte: nothing until a frame is complete.
    let mut d = eventstream::Decoder::default();
    let mut frames = Vec::new();
    for byte in a.iter().chain(&b) {
        d.push(&[*byte]);
        while let Some(f) = d.next_frame().unwrap() {
            frames.push(f);
        }
    }
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].header(":event-type"), Some("chunk"));
    assert_eq!(frames[0].payload, br#"{"bytes":"eA=="}"#);
    assert_eq!(frames[1].header(":event-type"), Some("metadata"));
    assert_eq!(d.pending(), 0);
    // Two frames in one read.
    let mut d = eventstream::Decoder::default();
    d.push(&[a.clone(), b.clone()].concat());
    assert!(
        d.next_frame().unwrap().is_some()
            && d.next_frame().unwrap().is_some()
            && d.next_frame().unwrap().is_none()
    );
    // A flipped payload byte fails the message CRC; a flipped length the prelude CRC.
    let mut bad = a.clone();
    let n = bad.len();
    bad[n - 6] ^= 1;
    let mut d = eventstream::Decoder::default();
    d.push(&bad);
    assert!(
        d.next_frame()
            .unwrap_err()
            .to_string()
            .contains("message checksum")
    );
    let mut bad = a.clone();
    bad[3] ^= 1;
    let mut d = eventstream::Decoder::default();
    d.push(&bad);
    assert!(
        d.next_frame()
            .unwrap_err()
            .to_string()
            .contains("prelude checksum")
    );
}

#[test]
fn event_stream_header_types() {
    // "n": int32 -2, "b": true, "u": uuid, "t": timestamp.
    let mut h = vec![1, b'n', 4];
    h.extend_from_slice(&(-2i32).to_be_bytes());
    h.extend_from_slice(&[1, b'b', 0, 1, b'u', 9]);
    h.extend_from_slice(&[7u8; 16]);
    h.extend_from_slice(&[1, b't', 8]);
    h.extend_from_slice(&1_700_000_000_000i64.to_be_bytes());
    let total = 12 + h.len() + 4;
    let mut f = Vec::new();
    f.extend_from_slice(&(total as u32).to_be_bytes());
    f.extend_from_slice(&(h.len() as u32).to_be_bytes());
    f.extend_from_slice(&crc32fast::hash(&f).to_be_bytes());
    f.extend_from_slice(&h);
    f.extend_from_slice(&crc32fast::hash(&f).to_be_bytes());
    let mut d = eventstream::Decoder::default();
    d.push(&f);
    let frame = d.next_frame().unwrap().unwrap();
    use eventstream::HeaderValue::*;
    assert_eq!(
        frame.headers,
        vec![
            ("n".into(), Int(-2)),
            ("b".into(), Bool(true)),
            ("u".into(), Uuid([7; 16])),
            ("t".into(), Timestamp(1_700_000_000_000))
        ]
    );
    assert!(frame.payload.is_empty());
}

// ---------------------------------------------------------------- AWS credentials

#[cfg(unix)]
#[tokio::test]
async fn aws_credentials_precedence_and_process_caching() {
    let home = tmp();
    let far = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let soon = (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339();
    let fresh = script(
        &home.join("bin/fresh"),
        &format!(
            r#"{{"Version": 1, "AccessKeyId": "AKPROC", "SecretAccessKey": "s", "SessionToken": "t", "Expiration": "{far}"}}"#
        ),
    );
    let expiring = script(
        &home.join("bin/expiring"),
        &format!(
            r#"{{"Version": 1, "AccessKeyId": "AKSOON", "SecretAccessKey": "s", "Expiration": "{soon}"}}"#
        ),
    );
    let cli = script(
        &home.join("bin/aws"),
        &format!(
            r#"{{"Version": 1, "AccessKeyId": "AKCLI", "SecretAccessKey": "s", "Expiration": "{far}"}}"#
        ),
    );
    write(
        &home.join(".aws/credentials"),
        "[default]\naws_access_key_id = AKFILE\naws_secret_access_key = secret\n",
    );
    write(
        &home.join(".aws/config"),
        &format!(
            "[default]\nregion = eu-west-1\n[profile proc]\ncredential_process = {fresh} --flag\nregion = us-west-2\n[profile soon]\ncredential_process = {expiring}\n[profile role]\nrole_arn = arn:aws:iam::1:role/x\nsource_profile = default\ns3 =\n  max_concurrent_requests = 1\n"
        ),
    );
    let cli_var = ("CHANTO_AWS_CLI", cli.as_str());
    let none = Settings::default();
    // Environment keys beat the default profile.
    let vars = Vars::new(
        &home,
        &[
            ("AWS_ACCESS_KEY_ID", "AKENV"),
            ("AWS_SECRET_ACCESS_KEY", "e"),
            ("AWS_SESSION_TOKEN", "st"),
            cli_var,
        ],
    );
    let r = aws::credentials(&none, &vars).await.unwrap();
    assert_eq!(
        (
            r.creds.access_key_id.as_str(),
            r.creds.session_token.as_deref(),
            r.source.as_str()
        ),
        ("AKENV", Some("st"), "env:AWS_ACCESS_KEY_ID")
    );
    assert_eq!(
        aws::source(&none, &vars).as_deref(),
        Some("env:AWS_ACCESS_KEY_ID")
    );
    // ...but a profile named in Chanto config beats the environment.
    let proc = Settings {
        profile: Some("proc".into()),
        ..Default::default()
    };
    let r = aws::credentials(&proc, &vars).await.unwrap();
    assert_eq!(
        (r.creds.access_key_id.as_str(), r.source.as_str()),
        ("AKPROC", "AWS profile proc (credential_process)")
    );
    assert_eq!(runs(&home.join("bin/fresh")), ["--flag"]);
    // Cached until shortly before it expires.
    aws::credentials(&proc, &vars).await.unwrap();
    assert_eq!(runs(&home.join("bin/fresh")).len(), 1);
    // Credentials within five minutes of expiry are refreshed every time.
    let soon_vars = Vars::new(&home, &[("AWS_PROFILE", "soon")]);
    assert_eq!(
        aws::credentials(&none, &soon_vars)
            .await
            .unwrap()
            .creds
            .access_key_id,
        "AKSOON"
    );
    aws::credentials(&none, &soon_vars).await.unwrap();
    assert_eq!(runs(&home.join("bin/expiring")).len(), 2);
    // Without env keys: the default profile's static keys.
    let vars = Vars::new(&home, &[cli_var]);
    let r = aws::credentials(&none, &vars).await.unwrap();
    assert_eq!(
        (
            r.creds.access_key_id.as_str(),
            r.creds.session_token.as_deref()
        ),
        ("AKFILE", None)
    );
    assert_eq!(
        aws::source(&none, &vars).as_deref(),
        Some("AWS profile default (static keys)")
    );
    // Assume-role profiles go through `aws configure export-credentials`.
    let vars = Vars::new(&home, &[("AWS_PROFILE", "role"), cli_var]);
    assert_eq!(
        aws::source(&none, &vars).as_deref(),
        Some("AWS profile role (aws CLI)")
    );
    assert_eq!(
        aws::credentials(&none, &vars)
            .await
            .unwrap()
            .creds
            .access_key_id,
        "AKCLI"
    );
    assert_eq!(
        runs(&home.join("bin/aws")),
        ["configure export-credentials --format process --profile role"]
    );
    // A missing profile named in config is an error, not a fallback.
    let missing = Settings {
        profile: Some("nope".into()),
        ..Default::default()
    };
    assert!(
        aws::credentials(&missing, &vars)
            .await
            .unwrap_err()
            .to_string()
            .contains("`nope` not found")
    );
    // Regions: config, then env, then the profile.
    assert_eq!(
        aws::region(&none, &Vars::new(&home, &[])).unwrap().0,
        "eu-west-1"
    );
    assert_eq!(
        aws::region(&none, &Vars::new(&home, &[("AWS_PROFILE", "proc")])).unwrap(),
        ("us-west-2".to_string(), "profile proc".to_string())
    );
    assert_eq!(
        aws::region(
            &none,
            &Vars::new(&home, &[("AWS_DEFAULT_REGION", "ap-south-1")])
        )
        .unwrap()
        .0,
        "ap-south-1"
    );
    assert_eq!(
        aws::region(
            &Settings {
                region: Some("ca-central-1".into()),
                ..Default::default()
            },
            &Vars::new(&home, &[("AWS_REGION", "x")])
        )
        .unwrap()
        .0,
        "ca-central-1"
    );
    // A Bedrock API key is reported first.
    assert!(
        aws::source(
            &none,
            &Vars::new(&home, &[("AWS_BEARER_TOKEN_BEDROCK", "k")])
        )
        .unwrap()
        .contains("AWS_BEARER_TOKEN_BEDROCK")
    );
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
async fn aws_sso_profile_reads_the_cli_token_cache() {
    let home = tmp();
    write(
        &home.join(".aws/config"),
        "[profile corp]\nsso_session = my-sso\nsso_account_id = 111\nsso_role_name = Dev\nregion = us-east-1\n[sso-session my-sso]\nsso_region = us-east-1\nsso_start_url = https://x.awsapps.com/start\n",
    );
    let (base, seen) = serve(vec![(200, r#"{"roleCredentials":{"accessKeyId":"AKSSO","secretAccessKey":"s","sessionToken":"t","expiration":4102444800000}}"#.into())]).await;
    let vars = Vars::new(
        &home,
        &[("AWS_PROFILE", "corp"), ("AWS_ENDPOINT_URL_SSO", &base)],
    );
    let p = aws::load_profile("corp", &vars);
    // sha1("my-sso")
    assert_eq!(
        aws::sso_cache_file(&p, &vars).unwrap(),
        home.join(".aws/sso/cache/0ad374308c5a4e22f723adf10145eafad7c4031c.json")
    );
    assert!(
        aws::credentials(&Settings::default(), &vars)
            .await
            .unwrap_err()
            .to_string()
            .contains("aws sso login --profile corp")
    );
    write(
        &aws::sso_cache_file(&p, &vars).unwrap(),
        r#"{"accessToken": "sso-token", "expiresAt": "2099-01-01T00:00:00Z"}"#,
    );
    let r = aws::credentials(&Settings::default(), &vars).await.unwrap();
    assert_eq!(
        (r.creds.access_key_id.as_str(), r.source.as_str()),
        ("AKSSO", "AWS profile corp (IAM Identity Center (SSO))")
    );
    let h = seen.lock().unwrap().headers[0].clone();
    assert!(
        h.starts_with("GET /federation/credentials?account_id=111&role_name=Dev "),
        "{h}"
    );
    assert!(h.contains("x-amz-sso_bearer_token: sso-token"));
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn aws_ini_and_process_output() {
    let ini = aws::parse_ini(
        "# c\n[default]\nregion=us-east-1\n[profile  dev ]\nAWS_ACCESS_KEY_ID = a=b\n; x\n  nested = 1\n",
    );
    assert_eq!(ini["default"]["region"], "us-east-1");
    assert_eq!(ini["profile dev"]["aws_access_key_id"], "a=b");
    assert!(!ini["profile dev"].contains_key("nested"));
    let c = aws::parse_process_output(r#"{"Version":1,"AccessKeyId":"a","SecretAccessKey":"b"}"#)
        .unwrap();
    assert!(c.expires.is_none() && c.fresh());
    assert!(
        aws::parse_process_output(r#"{"Version":2,"AccessKeyId":"a","SecretAccessKey":"b"}"#)
            .is_err()
    );
}

// ---------------------------------------------------------------- Google

/// A fresh RSA key as a PKCS#8 PEM, plus its public key (PKCS#1 DER).
pub(crate) fn rsa_pem() -> (String, Vec<u8>) {
    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::signature::KeyPair;
    let key = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
    let der = key.as_der().unwrap();
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, der.as_ref());
    let lines: Vec<&str> = b64
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    (
        format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            lines.join("\n")
        ),
        key.public_key().as_ref().to_vec(),
    )
}

#[test]
fn service_account_jwt_is_rs256_signed() {
    let (pem, public) = rsa_pem();
    let jwt = gcp::service_account_jwt(
        "sa@p.iam.gserviceaccount.com",
        &pem,
        Some("kid1"),
        "https://oauth2.example/token",
        1_700_000_000,
    )
    .unwrap();
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    let dec = |s: &str| {
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, s).unwrap()
    };
    let header: serde_json::Value = serde_json::from_slice(&dec(parts[0])).unwrap();
    let claims: serde_json::Value = serde_json::from_slice(&dec(parts[1])).unwrap();
    assert_eq!(
        (header["alg"].as_str(), header["kid"].as_str()),
        (Some("RS256"), Some("kid1"))
    );
    assert_eq!(claims["iss"], "sa@p.iam.gserviceaccount.com");
    assert_eq!(claims["aud"], "https://oauth2.example/token");
    assert_eq!(
        claims["scope"],
        "https://www.googleapis.com/auth/cloud-platform"
    );
    assert_eq!(
        claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
        3600
    );
    let key = aws_lc_rs::signature::UnparsedPublicKey::new(
        &aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256,
        &public,
    );
    key.verify(
        format!("{}.{}", parts[0], parts[1]).as_bytes(),
        &dec(parts[2]),
    )
    .expect("signature verifies");
    assert!(
        gcp::service_account_jwt(
            "x",
            "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----",
            None,
            "a",
            0
        )
        .is_err()
    );
}

#[tokio::test]
async fn adc_user_and_service_account_tokens_are_cached() {
    let home = tmp();
    let (base, seen) = serve(vec![
        (
            200,
            r#"{"access_token":"ya29.user","expires_in":3599}"#.into(),
        ),
        (
            200,
            r#"{"access_token":"ya29.short","expires_in":30}"#.into(),
        ),
        (
            200,
            r#"{"access_token":"ya29.again","expires_in":30}"#.into(),
        ),
    ])
    .await;
    // Authorized-user ADC at gcloud's default path, with a quota project.
    let adc = home.join(".config/gcloud/application_default_credentials.json");
    write(
        &adc,
        &format!(
            r#"{{"type":"authorized_user","client_id":"cid","client_secret":"cs","refresh_token":"rt","quota_project_id":"quota-p","token_uri":"{base}/token"}}"#
        ),
    );
    let vars = Vars::new(&home, &[]);
    assert!(
        matches!(gcp::load_adc(&vars).unwrap().1.unwrap(), gcp::Adc::AuthorizedUser { ref quota_project, .. } if quota_project.as_deref() == Some("quota-p"))
    );
    assert_eq!(
        gcp::project(&Settings::default(), &vars).unwrap(),
        ("quota-p".to_string(), "ADC quota project".to_string())
    );
    assert!(
        gcp::source(&vars)
            .unwrap()
            .starts_with("ADC user credentials")
    );
    assert_eq!(gcp::access_token(&vars).await.unwrap(), "ya29.user");
    assert_eq!(
        gcp::access_token(&vars).await.unwrap(),
        "ya29.user",
        "cached"
    );
    {
        let s = seen.lock().unwrap();
        assert_eq!(s.raw.len(), 1);
        let form = String::from_utf8_lossy(&s.raw[0]);
        assert!(
            form.contains("grant_type=refresh_token")
                && form.contains("refresh_token=rt")
                && form.contains("client_id=cid"),
            "{form}"
        );
    }
    // A token within five minutes of expiry is refreshed on the next request.
    let short = home.join("short.json");
    write(
        &short,
        &format!(
            r#"{{"type":"authorized_user","client_id":"cid","client_secret":"cs","refresh_token":"rt","token_uri":"{base}/token"}}"#
        ),
    );
    let v = Vars::new(
        &home,
        &[("GOOGLE_APPLICATION_CREDENTIALS", short.to_str().unwrap())],
    );
    assert_eq!(gcp::access_token(&v).await.unwrap(), "ya29.short");
    assert_eq!(gcp::access_token(&v).await.unwrap(), "ya29.again");
    assert_eq!(seen.lock().unwrap().raw.len(), 3);
    // Service account key: JWT-bearer grant to its token_uri; project from the key.
    let (sa_base, sa_seen) = serve(vec![(
        200,
        r#"{"access_token":"ya29.sa","expires_in":3600}"#.into(),
    )])
    .await;
    let (pem, _) = rsa_pem();
    let key = home.join("sa.json");
    write(&key, &serde_json::json!({"type": "service_account", "project_id": "sa-proj", "private_key_id": "k1", "private_key": pem, "client_email": "sa@sa-proj.iam.gserviceaccount.com", "token_uri": format!("{sa_base}/token")}).to_string());
    let v = Vars::new(
        &home,
        &[("GOOGLE_APPLICATION_CREDENTIALS", key.to_str().unwrap())],
    );
    assert_eq!(gcp::project(&Settings::default(), &v).unwrap().0, "sa-proj");
    assert_eq!(
        gcp::project(
            &Settings::default(),
            &Vars::new(
                &home,
                &[
                    ("GOOGLE_APPLICATION_CREDENTIALS", key.to_str().unwrap()),
                    ("GOOGLE_CLOUD_PROJECT", "env-p")
                ]
            )
        )
        .unwrap()
        .0,
        "env-p"
    );
    assert_eq!(gcp::access_token(&v).await.unwrap(), "ya29.sa");
    let form = String::from_utf8_lossy(&sa_seen.lock().unwrap().raw[0]).to_string();
    assert!(
        form.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer")
            && form.contains("assertion=ey"),
        "{form}"
    );
    let _ = std::fs::remove_dir_all(home);
}

#[cfg(unix)]
#[tokio::test]
async fn gcloud_fallback_and_project_sources() {
    let home = tmp();
    let gcloud = script(&home.join("bin/gcloud"), "ya29.gcloud");
    let adc = home.join("wif.json");
    write(&adc, r#"{"type":"external_account","audience":"x"}"#);
    let v = Vars::new(
        &home,
        &[
            ("GOOGLE_APPLICATION_CREDENTIALS", adc.to_str().unwrap()),
            ("CHANTO_GCLOUD_CLI", &gcloud),
        ],
    );
    assert_eq!(gcp::access_token(&v).await.unwrap(), "ya29.gcloud");
    assert_eq!(
        runs(&home.join("bin/gcloud")),
        ["auth application-default print-access-token"]
    );
    // The active gcloud configuration names the project.
    write(&home.join(".config/gcloud/active_config"), "work\n");
    write(
        &home.join(".config/gcloud/configurations/config_work"),
        "[core]\naccount = me@example.com\nproject = from-gcloud\n",
    );
    assert_eq!(
        gcp::project(&Settings::default(), &Vars::new(&home, &[])).unwrap(),
        ("from-gcloud".to_string(), "gcloud config".to_string())
    );
    assert_eq!(
        gcp::region(&Settings::default(), &Vars::new(&home, &[])),
        ("global".to_string(), "default".to_string())
    );
    assert_eq!(
        gcp::region(
            &Settings::default(),
            &Vars::new(&home, &[("CLOUD_ML_REGION", "us-east5")])
        )
        .0,
        "us-east5"
    );
    assert_eq!(gcp::host("global"), "https://aiplatform.googleapis.com");
    assert_eq!(
        gcp::host("europe-west1"),
        "https://europe-west1-aiplatform.googleapis.com"
    );
    let _ = std::fs::remove_dir_all(home);
}

// ---------------------------------------------------------------- Azure

#[test]
fn azure_endpoints_normalize() {
    let s = Settings::default();
    let v = |pairs: &[(&str, &str)]| Vars::new("/nonexistent", pairs);
    for e in [
        "https://r.openai.azure.com",
        "https://r.openai.azure.com/",
        "https://r.openai.azure.com/openai/v1/",
        "https://r.openai.azure.com/openai",
    ] {
        assert_eq!(
            azure::openai_base(&s, "", &v(&[("AZURE_OPENAI_ENDPOINT", e)])).unwrap(),
            "https://r.openai.azure.com/openai/v1"
        );
    }
    let cfg = Settings {
        resource: Some("cfg".into()),
        ..Default::default()
    };
    assert_eq!(
        azure::openai_base(
            &cfg,
            "",
            &v(&[("AZURE_OPENAI_ENDPOINT", "https://env.openai.azure.com")])
        )
        .unwrap(),
        "https://cfg.openai.azure.com/openai/v1"
    );
    assert!(azure::openai_base(&s, "", &v(&[])).is_err());
    for u in [
        "https://f.services.ai.azure.com/anthropic",
        "https://f.services.ai.azure.com/anthropic/v1",
        "https://f.services.ai.azure.com/",
    ] {
        assert_eq!(
            azure::foundry_root(&s, "", &v(&[("ANTHROPIC_FOUNDRY_BASE_URL", u)])).unwrap(),
            "https://f.services.ai.azure.com"
        );
    }
    assert_eq!(
        azure::foundry_root(&s, "", &v(&[("AZURE_FOUNDRY_RESOURCE", "f")])).unwrap(),
        "https://f.services.ai.azure.com"
    );
    let t = azure::parse_az_token(
        r#"{"accessToken":"eyJ","expiresOn":"2030-01-01 10:00:00.000000","expires_on":1893492000}"#,
    )
    .unwrap();
    assert_eq!((t.value.as_str(), t.expires), ("eyJ", Some(1893492000)));
    let t =
        azure::parse_az_token(r#"{"accessToken":"eyJ","expiresOn":"2030-01-01 10:00:00.000000"}"#)
            .unwrap();
    assert!(t.expires.unwrap() > 1_800_000_000);
}

#[tokio::test]
async fn entra_service_principal_token_is_cached() {
    let (base, seen) = serve(vec![(
        200,
        r#"{"access_token":"eyJ.sp","expires_in":3600}"#.into(),
    )])
    .await;
    let tenant = uuid::Uuid::new_v4().to_string();
    let v = Vars::new(
        "/nonexistent",
        &[
            ("AZURE_TENANT_ID", &tenant),
            ("AZURE_CLIENT_ID", "app"),
            ("AZURE_CLIENT_SECRET", "pw"),
        ],
    );
    assert_eq!(azure::entra_token_with(&v, &base).await.unwrap(), "eyJ.sp");
    assert_eq!(
        azure::entra_token_with(&v, &base).await.unwrap(),
        "eyJ.sp",
        "cached"
    );
    let s = seen.lock().unwrap();
    assert_eq!(s.raw.len(), 1);
    assert!(s.headers[0].starts_with(&format!("POST /{tenant}/oauth2/v2.0/token ")));
    let form = String::from_utf8_lossy(&s.raw[0]);
    assert!(
        form.contains("grant_type=client_credentials")
            && form.contains("scope=https%3A%2F%2Fcognitiveservices.azure.com%2F.default"),
        "{form}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn entra_az_cli_fallback() {
    let home = tmp();
    let az = script(
        &home.join("bin/az"),
        &format!(
            r#"{{"accessToken":"eyJ.az","expires_on":{}}}"#,
            now() + 3600
        ),
    );
    // The az cache entry is process-wide; this is the only test that fills it.
    let v = Vars::new(&home, &[("CHANTO_AZ_CLI", &az)]);
    assert_eq!(azure::entra_token(&v).await.unwrap(), "eyJ.az");
    assert_eq!(
        runs(&home.join("bin/az")),
        ["account get-access-token --resource https://cognitiveservices.azure.com --output json"]
    );
    let _ = std::fs::remove_dir_all(home);
}
