use super::*;

fn identity(seed: u8) -> NodeIdentity {
    NodeIdentity::from_secret_bytes(&[seed; 32])
}

fn spec() -> CardSpec {
    CardSpec {
        name: "DARM node".into(),
        description: "Memory-augmented chat over a DARM-ANN mesh".into(),
        version: "0.1.0".into(),
        interfaces: vec![Interface {
            url: "https://agent.example/a2a/v1".into(),
            protocol_binding: "JSONRPC".into(),
            protocol_version: A2A_VERSION.into(),
        }],
        skills: vec![Skill {
            id: "chat".into(),
            name: "Chat".into(),
            description: "Answers with recalled memories".into(),
            tags: vec!["chat".into()],
        }],
        streaming: false,
        provider: None,
        documentation_url: None,
    }
}

fn signed(seed: u8) -> Value {
    let id = identity(seed);
    let card = build_card(&spec(), &id.node_id(), &Hash32::digest("c", b"chain")).unwrap();
    sign_card(&id, card, None).unwrap()
}

#[test]
fn canonical_form_matches_the_a2a_specification_example() {
    // A2A specification 8.4.1, after default-value removal.
    let card = json!({
        "name": "Example Agent",
        "description": "An example agent that echoes messages.",
        "supportedInterfaces": [{
            "url": "https://example.com/a2a/v1",
            "protocolBinding": "JSONRPC",
            "protocolVersion": "1.0"
        }],
        "version": "1.0.0",
        "capabilities": {"streaming": false, "pushNotifications": false},
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": [{"id": "echo", "name": "Echo", "description": "Echoes the input.", "tags": ["echo"]}],
        "signatures": [{"protected": "x", "signature": "y"}]
    });
    assert_eq!(
        signing_payload(&card).unwrap(),
        r#"{"capabilities":{"pushNotifications":false,"streaming":false},"defaultInputModes":["text/plain"],"defaultOutputModes":["text/plain"],"description":"An example agent that echoes messages.","name":"Example Agent","skills":[{"description":"Echoes the input.","id":"echo","name":"Echo","tags":["echo"]}],"supportedInterfaces":[{"protocolBinding":"JSONRPC","protocolVersion":"1.0","url":"https://example.com/a2a/v1"}],"version":"1.0.0"}"#
    );
}

#[test]
fn canonicalization_follows_rfc_8785_for_strings_keys_and_integers() {
    // Keys sort by UTF-16 code units: U+10000 (surrogates D800..) sorts
    // before U+FFFF, the reverse of code-point order.
    let value = json!({"\u{ffff}": 1, "\u{10000}": 2, "b": [true, null, -7], "a": "é\n\"/\u{1}"});
    assert_eq!(
        canonicalize(&value).unwrap(),
        "{\"a\":\"é\\n\\\"/\\u0001\",\"b\":[true,null,-7],\"\u{10000}\":2,\"\u{ffff}\":1}"
    );
    // Numbers that could print differently elsewhere are refused.
    for bad in [json!(1.5), json!(9_007_199_254_740_992u64), json!(1e30)] {
        assert!(canonicalize(&json!({"n": bad})).is_err());
    }
    assert_eq!(
        canonicalize(&json!(9_007_199_254_740_991u64)).unwrap(),
        "9007199254740991"
    );
}

#[test]
fn ed25519_jws_matches_rfc_8037_appendix_a4() {
    // Seed, header, payload and signature from RFC 8037 A.4.
    let seed: [u8; 32] = URL_SAFE_NO_PAD
        .decode("nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A")
        .unwrap()
        .try_into()
        .unwrap();
    let id = NodeIdentity::from_secret_bytes(&seed);
    assert_eq!(
        URL_SAFE_NO_PAD.encode(id.node_id().as_bytes()),
        "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"
    );
    let input = signing_input("eyJhbGciOiJFZERTQSJ9", "Example of Ed25519 signing");
    let Signature::Ed25519(signature) = id.sign(&input);
    assert_eq!(
        URL_SAFE_NO_PAD.encode(signature),
        "hgyY0il_MGCjP0JzlnLWG1PPOt7-09PGcvMg3AIbQR6dWbhijcNR4ki4iylGjg5BhVsPt9g7sVvpAr_MuM0KAg"
    );
}

#[test]
fn a_signed_card_verifies_and_names_its_signer() {
    let card = signed(1);
    assert_eq!(verify_card(&card).unwrap(), vec![identity(1).node_id()]);
    let (node, chain) = darm_binding(&card).unwrap();
    assert_eq!(node, identity(1).node_id());
    assert_eq!(chain, Hash32::digest("c", b"chain"));
    let header: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(card["signatures"][0]["protected"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(header["alg"], "EdDSA");
    assert_eq!(header["typ"], "JOSE");
    assert_eq!(header["kid"], identity(1).node_id().to_hex());
    // Key order and whitespace of the served JSON do not matter.
    let reparsed: Value =
        serde_json::from_str(&serde_json::to_string_pretty(&card).unwrap()).unwrap();
    assert_eq!(verify_card(&reparsed).unwrap().len(), 1);
    // A second signer is reported after the first.
    let both = sign_card(&identity(2), card, Some("https://agent.example/jwks.json")).unwrap();
    assert_eq!(
        verify_card(&both).unwrap(),
        vec![identity(1).node_id(), identity(2).node_id()]
    );
}

#[test]
fn altered_stripped_or_foreign_signatures_vouch_for_nothing() {
    let card = signed(1);
    // Rewritten endpoint.
    let mut moved = card.clone();
    moved["supportedInterfaces"][0]["url"] = json!("https://evil.example/a2a");
    assert!(verify_card(&moved).unwrap().is_empty());
    // Added field.
    let mut extra = card.clone();
    extra["iconUrl"] = json!("https://evil.example/i.png");
    assert!(verify_card(&extra).unwrap().is_empty());
    // Signature removed, or not an array.
    let mut bare = card.clone();
    bare.as_object_mut().unwrap().remove("signatures");
    assert!(verify_card(&bare).unwrap().is_empty());
    // Header swapped to another node's id: that node did not sign.
    let mut swapped = card.clone();
    let other = signed(2);
    swapped["signatures"][0]["protected"] = other["signatures"][0]["protected"].clone();
    assert!(verify_card(&swapped).unwrap().is_empty());
    // Signatures by keys that are not DARM node ids are ignored, not errors.
    let mut foreign = card.clone();
    foreign["signatures"] = json!([{
        "protected": URL_SAFE_NO_PAD.encode(r#"{"alg":"ES256","kid":"key-1","typ":"JOSE"}"#),
        "signature": "AAAA"
    }, {"protected": "!!", "signature": 7}]);
    assert!(verify_card(&foreign).unwrap().is_empty());
    assert!(verify_card(&json!([1, 2])).is_err());
}

#[test]
fn empty_optional_fields_added_in_transit_do_not_break_a_signature() {
    let mut card = signed(1);
    card["securitySchemes"] = json!({});
    card["skills"][0]["examples"] = json!([]);
    card["iconUrl"] = Value::Null;
    assert_eq!(verify_card(&card).unwrap().len(), 1);
}

#[test]
fn cards_need_real_interfaces_and_skills() {
    let node = identity(1).node_id();
    let chain = Hash32::ZERO;
    let mut none = spec();
    none.interfaces.clear();
    assert!(build_card(&none, &node, &chain).is_err());
    let mut plain = spec();
    plain.interfaces[0].url = "http://agent.example/a2a".into();
    assert!(build_card(&plain, &node, &chain).is_err());
    let mut binding = spec();
    binding.interfaces[0].protocol_binding = "SOAP".into();
    assert!(build_card(&binding, &node, &chain).is_err());
    let mut skills = spec();
    skills.skills.clear();
    assert!(build_card(&skills, &node, &chain).is_err());
    let keys = jwks(&node);
    assert_eq!(keys["keys"][0]["kty"], "OKP");
    assert_eq!(keys["keys"][0]["kid"], node.to_hex());
}
