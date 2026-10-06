//! `secrets-v3`: the rules a pre-release review found missing (Korean labels,
//! credentials on a command line, `.netrc`, XML, Docker and kubeconfig files,
//! cookies, name/value pairs, bare provider tokens, webhook URLs), each with
//! what it must find and what it must leave alone. Precision first: a miss is
//! the documented limit of a pattern scanner, a false positive silently
//! damages the record.

use super::*;

/// A deterministic run of `n` letters and digits, in mixed case.
fn mixed(n: usize) -> String {
    "aB3dE5gH7jK9mN1pQ2rS4tU6vW8xY0zA"
        .chars()
        .cycle()
        .take(n)
        .collect()
}

/// A deterministic run of `n` letters only, in mixed case.
fn letters(n: usize) -> String {
    "aBcDeFgHiJkLmNoPqRsTuVwXyZ"
        .chars()
        .cycle()
        .take(n)
        .collect()
}

fn assert_redacts<A: AsRef<str>, B: AsRef<str>>(cases: &[(A, B)]) {
    for (input, expected) in cases {
        let (input, expected) = (input.as_ref(), expected.as_ref());
        let out = redact(input).0;
        assert_eq!(out, expected, "{input}");
        // Redaction is idempotent: the marker is not a secret.
        assert!(!contains_secret(&out), "a second pass matched: {out}");
    }
}

fn assert_untouched<S: AsRef<str>>(cases: &[S]) {
    for text in cases {
        let text = text.as_ref();
        let hits = scan(text);
        assert!(hits.is_empty(), "{text:?} was matched: {hits:?}");
        assert_eq!(redact(text), (text.to_string(), 0), "{text}");
    }
}

#[test]
fn korean_labels_with_a_value() {
    assert_redacts(&[
        (
            "비밀번호: hunter2abc",
            "비밀번호: [REDACTED:generic_assignment]",
        ),
        (
            "비밀번호=Admin123!",
            "비밀번호=[REDACTED:generic_assignment]",
        ),
        (
            "패스워드: s3cr3t!pw",
            "패스워드: [REDACTED:generic_assignment]",
        ),
        ("암호 : x9y8z7w6", "암호 : [REDACTED:generic_assignment]"),
        ("토큰: abc123def456", "토큰: [REDACTED:generic_assignment]"),
        ("비번: qwer1234", "비번: [REDACTED:generic_assignment]"),
        // A PIN is a password.
        ("비밀번호: 1234", "비밀번호: [REDACTED:generic_assignment]"),
        // A label inside a sentence, a particle glued to the value, a label
        // glued to an ASCII word.
        (
            "DB 비밀번호: P@ssw0rd 입니다",
            "DB 비밀번호: [REDACTED:generic_assignment] 입니다",
        ),
        (
            "토큰: abc123def456입니다",
            "토큰: [REDACTED:generic_assignment]입니다",
        ),
        (
            "API토큰: abcd1234ef",
            "API토큰: [REDACTED:generic_assignment]",
        ),
        (
            "비밀번호: \"hunter2abc\"",
            "비밀번호: \"[REDACTED:generic_assignment]\"",
        ),
    ]);
}

#[test]
fn korean_prose_that_is_not_a_credential_stays() {
    assert_untouched(&[
        "비밀번호: 필수 입력 항목입니다",
        "비밀번호를 입력하세요",
        "비밀번호 찾기: 이메일로 안내",
        "비밀번호: 변경됨",
        // What a language model counts is not a secret.
        "토큰: 1200",
        "토큰: 12345",
        "입력 토큰: 3400, 출력 토큰: 880",
        "토큰 수: 1,200",
        "암호화: AES-256-GCM",
        "암호화 방식: 사용",
        "비밀번호: ********",
        "비밀번호: ${DB_PASSWORD}",
        "비밀번호: <your-password>",
        "비밀번호: null",
        "비밀번호: password",
        "패스워드: abc",
        "비밀번호는 없음",
    ]);
}

#[test]
fn credentials_on_a_command_line() {
    const R: &str = "[REDACTED:command_line_credential]";
    let cases = [
        // mysql: the password is attached to `-p`.
        (
            "mysql -u root -pSECRET1 mydb",
            format!("mysql -u root -p{R} mydb"),
        ),
        (
            "mysqldump -uroot -p'my pass' db > out.sql",
            format!("mysqldump -uroot -p'{R}' db > out.sql"),
        ),
        (
            "/usr/bin/mariadb -h db -ps3cr3t -e 'select 1'",
            format!("/usr/bin/mariadb -h db -p{R} -e 'select 1'"),
        ),
        // curl.
        (
            "curl -u admin:s3cretpw https://example.com/api",
            format!("curl -u admin:{R} https://example.com/api"),
        ),
        (
            "curl --user admin:s3cretpw https://example.com",
            format!("curl --user admin:{R} https://example.com"),
        ),
        (
            "curl --user=admin:s3cretpw https://example.com",
            format!("curl --user=admin:{R} https://example.com"),
        ),
        (
            "curl -fsSu admin:s3cretpw https://example.com",
            format!("curl -fsSu admin:{R} https://example.com"),
        ),
        (
            "curl -uadmin:s3cretpw https://example.com",
            format!("curl -uadmin:{R} https://example.com"),
        ),
        (
            "curl -u \"admin:s3cretpw\" https://example.com",
            format!("curl -u \"admin:{R}\" https://example.com"),
        ),
        (
            "curl --proxy-user px:pw123456 -x http://proxy:3128 https://example.com",
            format!("curl --proxy-user px:{R} -x http://proxy:3128 https://example.com"),
        ),
        (
            "curl --oauth2-bearer abc123def456 https://example.com",
            format!("curl --oauth2-bearer {R} https://example.com"),
        ),
        // A command line that goes on over lines, and two credentials in one.
        (
            "curl -s \\\n  -u admin:s3cretpw \\\n  https://example.com",
            format!("curl -s \\\n  -u admin:{R} \\\n  https://example.com"),
        ),
        (
            "curl -u a:pw123456 --proxy-user c:pw654321 https://x.example",
            format!("curl -u a:{R} --proxy-user c:{R} https://x.example"),
        ),
        // sshpass: its own flag, not the `-p` of the ssh it runs.
        (
            "sshpass -p hunter2 ssh -p 2222 user@host",
            format!("sshpass -p {R} ssh -p 2222 user@host"),
        ),
        (
            "sshpass -p'my pass' scp a user@host:/tmp",
            format!("sshpass -p'{R}' scp a user@host:/tmp"),
        ),
        // Registries.
        (
            "docker login -u me -p s3cretpw registry.example.com",
            format!("docker login -u me -p {R} registry.example.com"),
        ),
        (
            "docker login registry.example.com --password hunterpw -u me",
            format!("docker login registry.example.com --password {R} -u me"),
        ),
        (
            "az login --service-principal -u app-id -p S3cr3t! --tenant t",
            format!("az login --service-principal -u app-id -p {R} --tenant t"),
        ),
        (
            "skopeo copy --src-creds me:pw123456 docker://a docker://b",
            format!("skopeo copy --src-creds me:{R} docker://a docker://b"),
        ),
        (
            "podman pull --creds=me:pw123456 quay.io/x/y",
            format!("podman pull --creds=me:{R} quay.io/x/y"),
        ),
        // openssl, htpasswd, redis, mongo.
        (
            "openssl enc -aes-256-cbc -pass pass:hunter2 -in a -out b",
            format!("openssl enc -aes-256-cbc -pass pass:{R} -in a -out b"),
        ),
        (
            "openssl pkcs12 -export -passout pass:abc123 -out a.p12",
            format!("openssl pkcs12 -export -passout pass:{R} -out a.p12"),
        ),
        (
            "htpasswd -b .htpasswd alice hunter2",
            format!("htpasswd -b .htpasswd alice {R}"),
        ),
        (
            "htpasswd -nbB alice hunter2",
            format!("htpasswd -nbB alice {R}"),
        ),
        (
            "htpasswd -bc -C 12 file bob pw123456",
            format!("htpasswd -bc -C 12 file bob {R}"),
        ),
        (
            "redis-cli -h cache -a s3cretpw ping",
            format!("redis-cli -h cache -a {R} ping"),
        ),
        (
            "mongosh --host db -u admin -p s3cretpw",
            format!("mongosh --host db -u admin -p {R}"),
        ),
    ];
    for (input, expected) in cases {
        let out = redact(input).0;
        assert_eq!(out, expected, "{input}");
        assert!(!contains_secret(&out), "a second pass matched: {out}");
    }
}

#[test]
fn a_command_line_credential_does_not_hide_the_rest_of_the_line() {
    // Another rule's hit inside the same command is still found, in order.
    let out = redact(
        "curl -H 'Authorization: Bearer abc123def456ghi789' -u me:pw123456 \
         https://api:tok3nvalue@host/x",
    )
    .0;
    assert_eq!(
        out,
        "curl -H 'Authorization: Bearer [REDACTED:authorization_header]' \
         -u me:[REDACTED:command_line_credential] https://[REDACTED:url_credentials]@host/x"
    );
}

#[test]
fn flags_that_are_not_credentials_stay() {
    assert_untouched(&[
        "mkdir -p dir/sub",
        "cp -p a b",
        "ssh -p 2222 host",
        "scp -P 2222 a host:b",
        "docker run -p 8080:80 nginx",
        "docker run -u 1000:1000 img",
        "docker exec web curl -s http://localhost",
        "sudo -u postgres psql",
        "git push -u origin main",
        "uniq -u",
        "tar -xpf a.tar",
        "rsync -avp a b",
        // mysql: a bare `-p` prompts, and `-p name` names a database.
        "mysql -u root -p",
        "mysql -u root -p mydb",
        "mysql -h db -P 3306 -u root",
        // curl without a password, or with one that is a reference or a word.
        "curl -u user https://example.com",
        "curl -u user:$PASS https://example.com",
        "curl -u user:\"$PASS\" https://example.com",
        "curl -u user:${TOKEN} https://example.com",
        "curl -u user:<password> https://example.com",
        "curl -u username:password https://example.com",
        "curl --oauth2-bearer $TOKEN https://example.com",
        "curl -s -o out.txt https://example.com",
        "docker login -p $TOKEN registry.example.com",
        "docker login --password-stdin",
        "sshpass -p $PW ssh host",
        "sshpass -f /run/secrets/pw ssh host",
        "az group create -n rg -l westus -p x1y2z3",
        "htpasswd .htpasswd alice",
        "htpasswd -n alice",
        "redis-cli -a",
        "redis-cli -h cache ping",
        "openssl rand -hex 16",
        "openssl req -new -pass env:PW",
        "openssl rsa -passin file:/run/secrets/pw",
        "openssl enc -pass pass:$PW",
        "openssl enc -pass pass:password",
        "mongosh --host db -u admin -p",
        // The words alone.
        "use curl -u to pass a user",
        "mysql is a database",
        "the docker login command",
    ]);
}

#[test]
fn netrc_passwords() {
    assert_redacts(&[
        (
            "machine api.example.com login bob password s3cretpw",
            "machine api.example.com login bob password [REDACTED:netrc_password]",
        ),
        (
            "machine example.com\n  login alice\n  password hunter2\n",
            "machine example.com\n  login alice\n  password [REDACTED:netrc_password]\n",
        ),
        (
            "default login anonymous password me@example.org",
            "default login anonymous password [REDACTED:netrc_password]",
        ),
        (
            "machine a login x password pw111111\nmachine b login y password pw222222",
            "machine a login x password [REDACTED:netrc_password]\nmachine b login y password [REDACTED:netrc_password]",
        ),
    ]);
    assert_untouched(&[
        "machine learning password reset flow",
        "machine api.example.com login bob password $NETRC_PASSWORD",
        "machine api.example.com login bob password <password>",
        "machine api.example.com login bob",
        "the machine password is stored elsewhere",
        "default password policy",
    ]);
}

#[test]
fn xml_elements_named_like_a_secret() {
    assert_redacts(&[
        (
            "<password>hunter2</password>",
            "<password>[REDACTED:generic_assignment]</password>",
        ),
        (
            "<server><username>deploy</username><password> s3cr3tpw </password></server>",
            "<server><username>deploy</username><password> [REDACTED:generic_assignment] </password></server>",
        ),
        (
            "<db-password>pw123456</db-password>",
            "<db-password>[REDACTED:generic_assignment]</db-password>",
        ),
        (
            "<apiKey>abcd1234efgh</apiKey>",
            "<apiKey>[REDACTED:generic_assignment]</apiKey>",
        ),
        (
            "<passphrase>correct-horse</passphrase>",
            "<passphrase>[REDACTED:generic_assignment]</passphrase>",
        ),
    ]);
    assert_untouched(&[
        "<password></password>",
        "<password/>",
        "<password>${env.DB_PASSWORD}</password>",
        "<password>{COQLCE6DU6GtcS5P=}</password>",
        "<password>your-password</password>",
        "<token>string</token>",
        "<token>$TOKEN</token>",
        "<key>name</key>",
        "<secret_name>db-credentials</secret_name>",
        "<passwordField>text</passwordField>",
        "<input type=\"password\" name=\"pw\">",
        "<password>two words</password>",
        "<password>never closed",
    ]);
}

#[test]
fn registry_and_kubeconfig_credentials() {
    let auth = "dXNlcjpwYXNzd29yZDEyMw==";
    assert_redacts(&[
        (
            format!("{{\"auths\":{{\"https://index.docker.io/v1/\":{{\"auth\":\"{auth}\"}}}}}}"),
            "{\"auths\":{\"https://index.docker.io/v1/\":{\"auth\":\"[REDACTED:registry_auth]\"}}}"
                .to_string(),
        ),
        (
            "\"identitytoken\": \"abc123def456ghi789\"".to_string(),
            "\"identitytoken\": \"[REDACTED:generic_assignment]\"".to_string(),
        ),
    ]);
    let key = mixed(64);
    assert_redacts(&[
        (
            format!("    client-key-data: {key}\n    user: x"),
            "    client-key-data: [REDACTED:client_key_data]\n    user: x".to_string(),
        ),
        (
            format!("client-certificate-data: {key}"),
            "client-certificate-data: [REDACTED:client_key_data]".to_string(),
        ),
        (
            format!("\"client-key-data\": \"{key}\""),
            "\"client-key-data\": \"[REDACTED:client_key_data]\"".to_string(),
        ),
    ]);
    assert_untouched(&[
        "\"auth\": \"basic\"",
        "\"auth\": \"oauth2\"",
        "\"auth\": \"authenticated-user-session\"",
        "\"auth\": \"none\"",
        "auth: dXNlcjpwYXNzd29yZDEyMw",
        "\"authorization\": \"required\"",
        "client-key-data: short",
        "client-key-data: ${KEY}",
        "certificate-authority-data: LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0tLS0tCk1JSUM=",
    ]);
    // The same through a parsed JSON value.
    let mut v = serde_json::json!({
        "auths": {"registry.example.com": {"auth": auth, "email": "a@b.c"}},
        "clusters": [{"name": "c", "client-key-data": key}],
        "auth": "basic"
    });
    assert_eq!(redact_value(&mut v), 2, "{v}");
    assert_eq!(
        v["auths"]["registry.example.com"]["auth"],
        "[REDACTED:registry_auth]"
    );
    assert_eq!(
        v["clusters"][0]["client-key-data"],
        "[REDACTED:client_key_data]"
    );
    assert_eq!(v["auth"], "basic");
}

#[test]
fn cookie_headers() {
    assert_redacts(&[
        (
            "Cookie: session=abcDEF123456; theme=dark",
            "Cookie: [REDACTED:cookie_header]",
        ),
        (
            "GET / HTTP/1.1\ncookie: sid=AbC123xYz987\nHost: x",
            "GET / HTTP/1.1\ncookie: [REDACTED:cookie_header]\nHost: x",
        ),
        (
            "Set-Cookie: sid=AbC123xYz987; Path=/; HttpOnly; Secure",
            "Set-Cookie: [REDACTED:cookie_header]",
        ),
        (
            "curl -H 'Cookie: sid=AbC123xYz987; lang=en' https://example.com",
            "curl -H 'Cookie: [REDACTED:cookie_header]' https://example.com",
        ),
        (
            "{\"Cookie\": \"sid=AbC123xYz987\"}",
            "{\"Cookie\": \"[REDACTED:cookie_header]\"}",
        ),
    ]);
    assert_untouched(&[
        "Cookie: theme=dark; lang=en",
        "Cookie: banner dismissed",
        "the Cookie: header is optional",
        "Set-Cookie: lang=en; Path=/",
        "Set-Cookie: theme=dark-mode-enabled",
        "Cookie: session=${SESSION}",
        "Cookie: <cookie>",
        "cookie settings: accept all",
        "Cookie:",
    ]);
    // A header held in a JSON member of its own.
    let mut v = serde_json::json!({
        "headers": {"cookie": "sid=AbC123xYz987", "set-cookie": "id=Zz9Yy8Xx7Ww6; Path=/", "accept": "text/html"}
    });
    assert_eq!(redact_value(&mut v), 2, "{v}");
    assert_eq!(v["headers"]["cookie"], "[REDACTED:cookie_header]");
    assert_eq!(v["headers"]["set-cookie"], "[REDACTED:cookie_header]");
    assert_eq!(v["headers"]["accept"], "text/html");
}

#[test]
fn name_value_pairs_with_a_secret_name() {
    const G: &str = "[REDACTED:generic_assignment]";
    let cases: Vec<(String, String)> = vec![
        (
            "{\"name\":\"DB_PASSWORD\",\"value\":\"hunter2abc\"}".into(),
            format!("{{\"name\":\"DB_PASSWORD\",\"value\":\"{G}\"}}"),
        ),
        (
            "{\"name\": \"API_KEY\", \"value\": \"k_12345678\"}".into(),
            format!("{{\"name\": \"API_KEY\", \"value\": \"{G}\"}}"),
        ),
        (
            "env:\n  - name: DB_PASSWORD\n    value: \"s3cr3tpw\"\n  - name: DB_HOST\n    value: db"
                .into(),
            format!(
                "env:\n  - name: DB_PASSWORD\n    value: \"{G}\"\n  - name: DB_HOST\n    value: db"
            ),
        ),
        (
            "{ name = \"DB_PASSWORD\", value = \"x1y2z3w4\" }".into(),
            format!("{{ name = \"DB_PASSWORD\", value = \"{G}\" }}"),
        ),
        (
            "<add key=\"DbPassword\" value=\"x1y2z3w4\" />".into(),
            format!("<add key=\"DbPassword\" value=\"{G}\" />"),
        ),
        (
            format!("{{\"name\":\"API_KEY\",\"value\":\"ghp_{}\"}}", mixed(36)),
            "{\"name\":\"API_KEY\",\"value\":\"[REDACTED:github_token]\"}".into(),
        ),
    ];
    assert_redacts(&cases);
    assert_untouched(&[
        "{\"name\":\"DB_HOST\",\"value\":\"db.internal\"}",
        "{\"name\":\"DB_PASSWORD\",\"valueFrom\":{\"secretKeyRef\":{\"name\":\"pw\",\"key\":\"p\"}}}",
        "{\"name\":\"SECRET_NAME\",\"value\":\"my-secret-store\"}",
        "{\"name\":\"DB_PASSWORD\",\"value\":\"${DB_PASSWORD}\"}",
        "{\"name\":\"TOKEN\",\"value\":\"\"}",
        "{\"name\":\"password\",\"value\":\"string\"}",
        "- name: API_KEY\n  value: <your key>",
        "name: API_KEY\ndescription: the key",
        "name = value",
        "key: API_KEY, value: settings.api_key",
    ]);
    // A parsed JSON value: the name and the value are separate strings.
    let mut v = serde_json::json!({
        "containerDefinitions": [{"environment": [
            {"name": "DB_PASSWORD", "value": "hunter2abc"},
            {"name": "DB_HOST", "value": "db.internal"},
            {"Name": "API_TOKEN", "Value": "abc123def456"},
            {"name": "TOKEN", "valueFrom": "arn:aws:secretsmanager:eu-west-1:1:secret:x"}
        ]}]
    });
    assert_eq!(redact_value(&mut v), 2, "{v}");
    let env = &v["containerDefinitions"][0]["environment"];
    assert_eq!(env[0]["value"], G);
    assert_eq!(env[1]["value"], "db.internal");
    assert_eq!(env[2]["Value"], G);
    assert_eq!(
        env[3]["valueFrom"],
        "arn:aws:secretsmanager:eu-west-1:1:secret:x"
    );
}

#[test]
fn npmrc_tokens() {
    let cases: Vec<(String, String)> = vec![
        (
            "//registry.npmjs.org/:\x5fauthToken=\x30b5d8f3a-1c2d-4e5f-8a9b-0c1d2e3f4a5b".into(),
            "//registry.npmjs.org/:\x5fauthToken=[REDACTED:generic_assignment]".into(),
        ),
        (
            "_auth=dXNlcjpwYXNzd29yZA==\nalways-auth=true".into(),
            "_auth=[REDACTED:generic_assignment]\nalways-auth=true".into(),
        ),
        (
            "//npm.example.com/:_password = cGFzc3dvcmQxMjM=".into(),
            "//npm.example.com/:_password = [REDACTED:generic_assignment]".into(),
        ),
        (
            format!("//registry.npmjs.org/:\x5fauthToken=npm_{}", mixed(36)),
            "//registry.npmjs.org/:\x5fauthToken=[REDACTED:npm_token]".into(),
        ),
    ];
    assert_redacts(&cases);
    assert_untouched(&[
        "//registry.npmjs.org/:\x5fauthToken=${NPM_TOKEN}",
        "//npm.pkg.github.com/:_authToken=$GH_TOKEN",
        "//registry.npmjs.org/:\x5fauthToken=",
        "registry=https://registry.npmjs.org/",
        "_auth_helper=/usr/bin/helper",
        "_authToken=YOUR_NPM_TOKEN",
    ]);
}

#[test]
fn bare_provider_tokens_and_webhook_urls() {
    let discord_token = mixed(68);
    let cases = [
        (
            format!("access token ya29.a0{}", mixed(80)),
            "access token [REDACTED:google_oauth_token]".to_string(),
        ),
        (
            format!("HF_TOKEN=hf_{}", letters(34)),
            "HF_TOKEN=[REDACTED:huggingface_token]".to_string(),
        ),
        (
            format!("glpat-{}", mixed(20)),
            "[REDACTED:gitlab_token]".to_string(),
        ),
        (
            format!("glpat-{}.01.abc123", mixed(20)),
            "[REDACTED:gitlab_token]".to_string(),
        ),
        (
            format!("gsk_{}", mixed(52)),
            "[REDACTED:groq_api_key]".to_string(),
        ),
        (
            format!("xai-{}", mixed(80)),
            "[REDACTED:xai_api_key]".to_string(),
        ),
        (
            format!("ntn_{}", mixed(46)),
            "[REDACTED:notion_token]".to_string(),
        ),
        (
            format!("X-Shopify-Access-Token: shpat_{}", "0123456789abcdef".repeat(2)),
            "X-Shopify-Access-Token: [REDACTED:shopify_token]".to_string(),
        ),
        (
            format!("whsec_{}", mixed(32)),
            "[REDACTED:stripe_webhook_secret]".to_string(),
        ),
        (
            "post to https://\x68ooks.slack.com/services/T01234567/B01234567/abcdefghijklmnopqrstuvwx now"
                .to_string(),
            "post to https://\x68ooks.slack.com/services/[REDACTED:slack_webhook] now".to_string(),
        ),
        (
            format!("https://discord.com/api/webhooks/123456789012345678/{discord_token}"),
            "https://discord.com/api/webhooks/[REDACTED:discord_webhook]".to_string(),
        ),
        (
            format!("https://discordapp.com/api/v10/webhooks/123456789012345678/{discord_token}?wait=true"),
            "https://discordapp.com/api/v10/webhooks/[REDACTED:discord_webhook]?wait=true".to_string(),
        ),
        (
            format!("TELEGRAM_BOT_TOKEN: 123456789:AAH{}", mixed(32)),
            "TELEGRAM_BOT_TOKEN: [REDACTED:telegram_bot_token]".to_string(),
        ),
        (
            format!("https://api.telegram.org/bot123456789:AAH{}/sendMessage", mixed(32)),
            "https://api.telegram.org/bot[REDACTED:telegram_bot_token]/sendMessage".to_string(),
        ),
    ];
    for (input, expected) in cases {
        let out = redact(&input).0;
        assert_eq!(out, expected, "{input}");
        assert!(!contains_secret(&out), "a second pass matched: {out}");
    }
    assert_untouched(&[
        "from huggingface_hub import hf_hub_download",
        "export HF_HUB_ENABLE_HF_TRANSFER=1",
        "hf_transfer",
        format!("hf_{}", "abcdefghij".repeat(4)).as_str(),
        format!("hf_{}", "ABCDEFGHIJ".repeat(4)).as_str(),
        format!("hf_{}_{}", letters(20), letters(20)).as_str(),
        format!("gsk_{}", "abcdefghij".repeat(5)).as_str(),
        format!("gsk_{}_{}", mixed(30), mixed(30)).as_str(),
        "xai-sdk-python-grpc-client-library-for-the-xai-api-1.2.3",
        format!("xai-{}", "a".repeat(50)).as_str(),
        "ya29.short",
        "glpat-short",
        "ntn_abc",
        "the shpat_ prefix marks a Shopify admin token",
        "whsec_ is the Stripe webhook secret prefix",
        "https://\x68ooks.slack.com/services/",
        "https://\x68ooks.slack.com/services/T00000000/B00000000/XXXXXXXXXXXXXXXXXXXXXXXX",
        "https://\x68ooks.slack.com/services/T01/B01/abc",
        "https://discord.com/api/webhooks/123/abc",
        "https://discord.com/api/v10/channels/123456789012345678/messages",
        "1234567890:ABC",
        format!("12345678901:AAH{}", mixed(32)).as_str(),
        format!("123456789:{}", "1234567890".repeat(4).get(..35).unwrap()).as_str(),
        "1700000000:000000000000000000000000000000000000",
        "https://api.telegram.org/bot/getMe",
        "https://api.telegram.org/botfather",
    ]);
}

#[test]
fn a_keyword_argument_that_passes_a_variable_along_is_a_reference() {
    assert_untouched(&[
        "connect(host, password=password)",
        "client = Client(token=token, secret=secret)",
        "login(user, password=Password)",
        "f(api_key=apiKey)",
        "f(apiKey=api_key)",
        "requests.get(url, auth=auth, token=token)",
        "token: token",
        "password = password",
        // Terraform and the like: a reference, not a literal.
        "password = random_password.db.result",
        "password = var.db_password",
        "password = data.aws_secretsmanager_secret_version.db.secret_string",
    ]);
    // A literal that happens to be near the key's name is still one.
    assert_redacts(&[
        (
            "connect(password=hunter2)",
            "connect(password=[REDACTED:generic_assignment])",
        ),
        (
            "DB_PASSWORD=password",
            "DB_PASSWORD=[REDACTED:generic_assignment]",
        ),
        (
            "connect(password=\"password\")",
            "connect(password=\"[REDACTED:generic_assignment]\")",
        ),
        (
            "set(token=other_token_value1)",
            "set(token=[REDACTED:generic_assignment])",
        ),
    ]);
}

/// Every new rule scans in time proportional to the text, on its own long
/// repeated lookalikes (the shape that made the assignment rule quadratic).
#[test]
fn the_new_rules_are_linear_on_pathological_texts() {
    let n = 60_000;
    let texts: Vec<String> = vec![
        "비밀번호: ".repeat(n),
        "토큰=".repeat(n),
        "curl -u ".repeat(n),
        "curl ".repeat(n),
        "curl -u a:b ".repeat(n),
        "mysql -p".repeat(n),
        "sshpass -p ".repeat(n),
        "docker login -p ".repeat(n),
        "docker ".repeat(n),
        "htpasswd -b ".repeat(n),
        "openssl -pass ".repeat(n),
        "redis-cli -a ".repeat(n),
        "-u ".repeat(n),
        "-p ".repeat(n),
        "machine ".repeat(n),
        "machine h login ".repeat(n),
        "machine h login u password ".repeat(n),
        "default login ".repeat(n),
        "<password>".repeat(n),
        "<password>x".repeat(n),
        "<a>".repeat(n),
        "\"auth\":\"".repeat(n),
        "auth\":".repeat(n),
        "client-key-data: ".repeat(n),
        format!("client-key-data: {}", "A".repeat(1_000_000)),
        format!("auth\":\"{}", "A".repeat(1_000_000)),
        "Cookie: ".repeat(n),
        "Cookie:".repeat(n),
        "Set-Cookie: a=b; ".repeat(n),
        format!("Cookie: {}", "a=Bcdefghij1; ".repeat(n)),
        "{\"name\":\"DB_PASSWORD\",\"value\":".repeat(n),
        "name: API_TOKEN\n  value: ".repeat(n),
        "name=API_TOKEN ".repeat(n),
        "name:".repeat(n),
        "_authToken=".repeat(n),
        "_auth".repeat(n),
        "ya29.".repeat(n),
        "glpat-".repeat(n),
        "hf_".repeat(n),
        "gsk_".repeat(n),
        "xai-".repeat(n),
        "https://\x68ooks.slack.com/services/".repeat(n),
        "https://discord.com/api/webhooks/".repeat(n),
        "123456789:".repeat(n),
        "bot123456789:".repeat(n),
        "1".repeat(1_000_000),
        format!("hf_{}", "a".repeat(1_000_000)),
        format!("ya29.{}", "a".repeat(1_000_000)),
    ];
    for text in &texts {
        let started = std::time::Instant::now();
        let _ = scan(text);
        let _ = contains_secret(text);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "masking {} bytes of {:?}… took {:?}",
            text.len(),
            text.chars().take(24).collect::<String>(),
            started.elapsed()
        );
    }
}

/// A whole document of the new rules' negatives and the old corpus's shapes,
/// scanned as one text: nothing in it may match, however the lines combine.
#[test]
fn lookalikes_in_one_document_stay_untouched() {
    let doc = [
        "mkdir -p out && cp -p a out/ && ssh -p 22 host",
        "docker run -p 8080:80 -u 1000:1000 nginx",
        "토큰: 1200 입력 토큰: 3400",
        "machine learning password reset",
        "<token>string</token>",
        "{\"name\":\"DB_HOST\",\"value\":\"db\"}",
        "Cookie: theme=dark; lang=en",
        "connect(password=password)",
        "https://\x68ooks.slack.com/services/T00000000/B00000000/XXXXXXXXXXXXXXXXXXXXXXXX",
    ]
    .join("\n");
    assert_eq!(redact(&doc).1, 0, "{}", redact(&doc).0);
}

// -- metadata ---------------------------------------------------------------

fn event_with_metadata() -> Event {
    use crate::event::Provider;
    use crate::{CaptureMode, DeviceId, EventKind, PortablePath, ProjectRef};
    let device = DeviceId::derive(&["metadata-test"]);
    let mut ev = Event::new(
        device,
        Provider::ClaudeCode,
        "PostToolUse",
        EventKind::ToolCallFinished,
        ProjectRef::derive("/work/app", None, &device),
        "s1",
        CaptureMode::LocalSemantic,
        "test",
    );
    ev.paths = vec![PortablePath::from_raw(
        "/work/app/src/main.rs",
        Some("/work/app"),
    )];
    ev
}

#[test]
fn metadata_strings_lose_the_secret_span_and_keep_the_rest() {
    let github = format!("ghp_{}", mixed(36));
    let stripe = format!("\x73k_live_{}", mixed(24));
    let mut ev = event_with_metadata();
    ev.paths = vec![crate::PortablePath {
        original: format!("/work/app/{github}.txt"),
        logical: format!("/work/app/{github}.txt"),
        repo_relative: Some(format!("{github}.txt")),
        ..Default::default()
    }];
    ev.project.root = format!("/work/{stripe}/app");
    ev.project.name = format!("acme/{stripe}");
    ev.project.repo_remote = Some(format!("https://deploy:{github}@github.com/acme/app.git"));
    ev.project.branch = Some("feat/AKIAIOSFODNN7EXAMPLE-fix".to_string());
    ev.agent.model = Some(format!("claude-{stripe}"));
    ev.tool = Some(crate::event::ToolRef {
        name: format!("mcp-{stripe}"),
        category: crate::event::ToolCategory::Other,
        call_id: Some("toolu_01A".into()),
    });
    let before_ids = (
        ev.event_id,
        ev.project.project_id,
        ev.session_id,
        ev.provider_session_id.clone(),
    );
    let stats = redact_event_metadata(&mut ev);
    assert_eq!(
        ev.paths[0].original,
        "/work/app/[REDACTED:github_token].txt"
    );
    assert_eq!(ev.paths[0].logical, "/work/app/[REDACTED:github_token].txt");
    assert_eq!(
        ev.paths[0].repo_relative.as_deref(),
        Some("[REDACTED:github_token].txt")
    );
    assert_eq!(ev.project.root, "/work/[REDACTED:stripe_key]/app");
    assert_eq!(ev.project.name, "acme/[REDACTED:stripe_key]");
    assert_eq!(
        ev.project.repo_remote.as_deref(),
        Some("https://[REDACTED:url_credentials]@github.com/acme/app.git")
    );
    assert_eq!(
        ev.project.branch.as_deref(),
        Some("feat/[REDACTED:aws_access_key_id]-fix")
    );
    assert_eq!(
        ev.agent.model.as_deref(),
        Some("claude-[REDACTED:stripe_key]")
    );
    assert_eq!(
        ev.tool.as_ref().map(|t| t.name.as_str()),
        Some("mcp-[REDACTED:stripe_key]")
    );
    assert_eq!(stats.spans, 9, "{stats:?}");
    // Identity is derived from the original text and does not move.
    assert_eq!(
        before_ids,
        (
            ev.event_id,
            ev.project.project_id,
            ev.session_id,
            ev.provider_session_id.clone()
        )
    );
    assert_eq!(
        ev.tool.as_ref().unwrap().call_id.as_deref(),
        Some("toolu_01A")
    );
    // A second pass finds nothing.
    assert!(redact_event_metadata(&mut ev).is_empty());
}

#[test]
fn ordinary_metadata_is_left_byte_for_byte() {
    let mut ev = event_with_metadata();
    let sha = "3f786850e387550fdab836ed7e6dc881de23001b";
    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    ev.paths = [
        "/work/app/src/auth/password.rs",
        "/work/app/token_count.rs",
        "C:\\Users\\dev\\project\\src\\main.rs",
        "C:/Users/dev/project/src/main.rs",
        "//server/share/project/api_key_test.rs",
        "/tmp/a=b/c:d",
        "~/projects/mask-it/sk-learn/README.md",
    ]
    .iter()
    .map(|p| crate::PortablePath::from_raw(p, Some("/work/app")))
    .collect();
    ev.project.root = "/work/app".to_string();
    ev.project.name = "acme/app".to_string();
    ev.project.repo_remote = Some("git@github.com:acme/app.git".to_string());
    ev.project.branch = Some("feature/password-reset".to_string());
    ev.project.head = Some(sha.to_string());
    ev.agent.model = Some("claude-opus-4-1-20250805".to_string());
    ev.agent.agent_type = Some("general-purpose".to_string());
    ev.provider_session_id = uuid.to_string();
    let before = serde_json::to_string(&ev).unwrap();
    let stats = redact_event_metadata(&mut ev);
    assert!(stats.is_empty(), "{stats:?}");
    assert_eq!(serde_json::to_string(&ev).unwrap(), before);
    for remote in [
        "https://github.com/acme/app",
        "https://github.com/acme/app.git",
        "ssh://git@github.com/acme/app.git",
        "https://gitlab.com/group/sub/repo.git",
    ] {
        ev.project.repo_remote = Some(remote.to_string());
        assert!(redact_event_metadata(&mut ev).is_empty(), "{remote}");
        assert_eq!(ev.project.repo_remote.as_deref(), Some(remote));
    }
}

#[test]
fn a_panicking_metadata_scan_blanks_what_it_could_not_check() {
    // The guard is what the gate calls; make the unguarded scan fail by
    // handing it a string it cannot panic on, and check the guard's wiring
    // through the same code the fallback uses.
    let mut ev = event_with_metadata();
    ev.project.branch = Some("main".into());
    for s in metadata_strings(&mut ev) {
        *s = "[REDACTED:scan_failed]".to_string();
    }
    assert_eq!(ev.project.root, "[REDACTED:scan_failed]");
    assert_eq!(ev.project.branch.as_deref(), Some("[REDACTED:scan_failed]"));
    assert_eq!(ev.paths[0].logical, "[REDACTED:scan_failed]");
}

// -- robustness -------------------------------------------------------------

/// Whatever text is scanned, the hits are in order, do not overlap, lie on
/// character boundaries and inside the text, and redaction does not panic.
fn assert_sane(text: &str) {
    let hits = scan(text);
    let mut last = 0;
    for h in &hits {
        assert!(
            h.start >= last && h.end > h.start && h.end <= text.len(),
            "{h:?} in {text:?}"
        );
        assert!(
            text.is_char_boundary(h.start) && text.is_char_boundary(h.end),
            "{h:?} splits a character of {text:?}"
        );
        last = h.end;
    }
    let (out, n) = redact(text);
    assert_eq!(n, hits.len());
    assert_eq!(contains_secret(text), !hits.is_empty(), "{text:?}");
    assert!(out.len() <= text.len() + 48 * hits.len(), "{out}");
}

#[test]
fn every_prefix_of_every_new_rules_example_scans_without_panicking() {
    let key = mixed(64);
    let samples = [
        "mysql -u root -p'pw 12345' db; curl -u 한글:pw12345 https://x | sed 's/a/b/'".to_string(),
        "sshpass -p 'hunter 2' ssh -p 22 host && docker login -p hunter2 reg".to_string(),
        "machine api.example.com
  login bob
  password s3cretpw
".to_string(),
        "<server><password> s3cr3tpw 한글 </password></server><token>x</token>".to_string(),
        format!("{{\"auths\":{{\"r\":{{\"auth\":\"dXNlcjpwYXNzd29yZDEyMw==\"}}}},\"client-key-data\":\"{key}\"}}"),
        "Cookie: sid=AbC123xYz987; theme=dark\nSet-Cookie: id=Zz9Yy8Xx7Ww6; Path=/; HttpOnly".replace("\\n", "\n"),
        "{\"name\":\"DB_PASSWORD\",\"value\":\"hunter2abc\"} - name: API_KEY\n  value: \"k_12345678\"".to_string(),
        "//registry.npmjs.org/:\x5fauthToken=0b5d8f3a-1c2d-4e5f-8a9b _auth=dXNlcjpwYXNz".to_string(),
        format!("ya29.a0{} hf_{} glpat-{} https://\x68ooks.slack.com/services/T01234567/B01234567/abcdefghijklmnopqrstuvwx", mixed(40), letters(34), mixed(20)),
        format!("https://discord.com/api/v10/webhooks/123456789012345678/{} bot123456789:AAH{}/send", mixed(68), mixed(32)),
        "비밀번호: 한글만 토큰=abc123입니다 암호 : x9y8z7w6 비번=🚀🚀🚀🚀".to_string(),
        "curl --user=\"a:b\" --oauth2-bearer \"tok1234\" \\\n -u x:y12345".to_string(),
        "htpasswd -bc -C 12 f u p\nopenssl enc -pass pass:\"a b\" -passin pass:한글12".to_string(),
    ];
    for sample in &samples {
        for (i, _) in sample.char_indices() {
            assert_sane(&sample[..i]);
        }
        assert_sane(sample);
    }
}

#[test]
fn a_value_cut_by_the_cap_inside_a_multibyte_character_is_not_sliced() {
    // The scan of an unclosed element stops at the cap, which can fall inside
    // a character; it must not slice there.
    for fill in ["한", "é", "🚀"] {
        for n in [100, 170, 171, 172, 255, 256, 257, 300] {
            let long = fill.repeat(n);
            for text in [
                format!("<password>{long}"),
                format!("<password>{long}</password>"),
                format!("비밀번호: {long}"),
                format!("password={long}"),
                format!("Cookie: sid=A1{long}"),
                format!("machine h login u password {long}"),
                format!("curl -u a:{long} https://x"),
                format!("{{\"name\":\"DB_PASSWORD\",\"value\":\"{long}\"}}"),
                format!("_authToken={long}"),
            ] {
                assert_sane(&text);
            }
        }
    }
}

/// A seeded shuffle of the rules' trigger fragments, ASCII and not.
#[test]
fn random_text_made_of_rule_fragments_scans_sanely() {
    let fragments = [
        "curl ",
        "mysql ",
        "-u ",
        "-p",
        "-pass ",
        "pass:",
        " a:b ",
        "docker ",
        "login ",
        "sshpass ",
        "htpasswd ",
        "-b ",
        "machine ",
        "default ",
        "password ",
        "login ",
        "Cookie: ",
        "Set-Cookie: ",
        "sid=Ab1cD2eF3; ",
        "<password>",
        "</password>",
        "<token>",
        "name",
        "key",
        "value",
        "\"",
        "'",
        ":",
        "=",
        ",",
        "\n",
        " ",
        "\\",
        "한",
        "🚀",
        "비밀번호",
        "토큰",
        "ya29.",
        "hf_",
        "glpat-",
        "xai-",
        "_authToken",
        "auth",
        "client-key-data",
        "123456789:",
        "\x68ooks.slack.com/services/",
        "discord.com/api/webhooks/",
        "bot",
        "AKIA",
        "ghp_",
        "Authorization: Bearer ",
        "://",
        "@",
        "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6",
        "0123456789",
    ];
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..3000 {
        let len = (next() % 40 + 1) as usize;
        let text: String = (0..len)
            .map(|_| fragments[(next() % fragments.len() as u64) as usize])
            .collect();
        assert_sane(&text);
    }
}
