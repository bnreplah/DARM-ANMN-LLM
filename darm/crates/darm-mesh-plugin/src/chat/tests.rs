use super::*;
use crate::config::DarmConfig;
use darm_core::NodeIdentity;
use std::sync::Mutex as StdMutex;

fn shared_node(config: DarmConfig) -> Arc<Mutex<DarmNode>> {
    let identity = NodeIdentity::from_secret_bytes(&[3; 32]);
    Arc::new(Mutex::new(
        DarmNode::ephemeral(config, identity, now_ms()).unwrap(),
    ))
}

fn candidate(model: &str, node: &str) -> VirtualModelCandidate {
    serde_json::from_value(json!({"model_id": model, "target_node_id": node})).unwrap()
}

fn invocation(request: Value, candidates: Vec<VirtualModelCandidate>) -> VirtualModelInvocation {
    VirtualModelInvocation {
        request,
        candidates,
        response_adapter: String::new(),
    }
}

fn completion(text: &str) -> HostInferenceResponse {
    HostInferenceResponse {
        status_code: 200,
        body: json!({"model": "concrete", "choices": [{"message": {"role": "assistant", "content": text}}]}),
        served_by: Some("peer-x".into()),
    }
}

#[test]
fn options_are_parsed_and_stripped() {
    let mut request =
        json!({"messages": [], "darm": {"remember": false, "namespace": "team", "top_k": 2}});
    let options = take_options(&mut request, true, 5).unwrap();
    assert_eq!(
        options,
        TurnOptions {
            recall: true,
            remember: false,
            namespace: "team".into(),
            top_k: 2,
            tier: None,
        }
    );
    assert!(
        request.get("darm").is_none(),
        "extension never reaches a model"
    );
    assert!(take_options(&mut json!({"darm": {"top_k": 999}}), true, 5).is_err());
    assert!(take_options(&mut json!({"darm": "yes"}), true, 5).is_err());
}

#[test]
fn tier_hint_narrows_without_emptying_the_pool() {
    let sized = |model: &str, b: f64| -> VirtualModelCandidate {
        serde_json::from_value(
            json!({"model_id": model, "target_node_id": "n", "parameter_count_b": b}),
        )
        .unwrap()
    };
    let pool = || {
        vec![
            sized("tiny-1b", 1.0),
            sized("mid-14b", 14.0),
            sized("xl-70b", 70.0),
        ]
    };
    let request = json!({"messages": []});
    let names =
        |c: Vec<VirtualModelCandidate>| c.into_iter().map(|c| c.model_id).collect::<Vec<_>>();
    assert_eq!(
        names(eligible(&request, pool(), Some(Tier::Tiny))),
        ["tiny-1b"]
    );
    assert_eq!(
        names(eligible(&request, pool(), Some(Tier::Mid))),
        ["mid-14b"]
    );
    assert_eq!(
        names(eligible(&request, pool(), Some(Tier::Consolidator))),
        ["xl-70b"]
    );
    assert_eq!(eligible(&request, pool(), None).len(), 3);
    // No model in the tier: the hint is ignored rather than failing the turn.
    let only_big = vec![sized("xl-70b", 70.0)];
    assert_eq!(
        names(eligible(&request, only_big, Some(Tier::Tiny))),
        ["xl-70b"]
    );
    let mut req = json!({"darm": {"tier": "mid"}});
    assert_eq!(
        take_options(&mut req, true, 5).unwrap().tier,
        Some(Tier::Mid)
    );
    assert!(take_options(&mut json!({"darm": {"tier": "huge"}}), true, 5).is_err());
    assert_eq!(
        take_options(&mut json!({"darm": {"tier": "auto"}}), true, 5)
            .unwrap()
            .tier,
        None
    );
}

#[test]
fn memory_block_goes_after_operator_system_prompt() {
    let mut request = json!({"messages": [
        {"role": "system", "content": "operator"},
        {"role": "user", "content": [{"type": "text", "text": "hi"}]}
    ]});
    inject_memory(&mut request, "MEM".into());
    let roles: Vec<_> = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap_or("[parts]").to_string())
        .collect();
    assert_eq!(roles, ["operator", "MEM", "[parts]"]);
    assert_eq!(last_user_text(&request).as_deref(), Some("hi"));
}

#[tokio::test]
async fn turn_recalls_routes_with_fallback_and_learns() {
    let node = shared_node(DarmConfig::default());
    let seen: Arc<StdMutex<Vec<HostInferenceRequest>>> = Arc::default();

    // Turn 1: the first-ranked route fails, the other answers; the exchange
    // is remembered.
    let seen_1 = Arc::clone(&seen);
    let (response, _) = handle(
        invocation(
            json!({"model": "darm", "stream": true, "messages": [{"role": "user", "content": "Which port serves the mesh OpenAI API?"}]}),
            vec![candidate("broken", "n1"), candidate("good", "n2")],
        ),
        Arc::clone(&node),
        move |request| {
            let seen = Arc::clone(&seen_1);
            async move {
                seen.lock().unwrap().push(request.clone());
                if request.model_id == "broken" {
                    anyhow::bail!("connection reset")
                }
                Ok(completion("The mesh OpenAI API listens on port 9337."))
            }
        },
    )
    .await;
    assert_eq!(response.status_code, 200);
    assert!(response.event_stream);
    assert_eq!(response.body["model"], "darm");
    assert!(
        response
            .headers
            .iter()
            .any(|(k, _)| k == "x-darm-memory-id")
    );
    for request in seen.lock().unwrap().iter() {
        assert!(request.request.get("stream").is_none());
        assert!(request.request.get("darm").is_none());
    }

    // Turn 2: the memory is recalled into the prompt, and the trail laid by
    // turn 1 steers routing toward the working model.
    seen.lock().unwrap().clear();
    let seen_2 = Arc::clone(&seen);
    let (response, _) = handle(
        invocation(
            json!({"messages": [{"role": "user", "content": "remind me: what port is the OpenAI API on?"}]}),
            vec![candidate("broken", "n1"), candidate("good", "n2")],
        ),
        Arc::clone(&node),
        move |request| {
            let seen = Arc::clone(&seen_2);
            async move {
                seen.lock().unwrap().push(request.clone());
                Ok(completion("9337"))
            }
        },
    )
    .await;
    assert_eq!(response.status_code, 200);
    let header = |name: &str| {
        response
            .headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(header("x-darm-memories").as_deref(), Some("1"));
    let first = &seen.lock().unwrap()[0];
    let system = first.request["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("<darm_memories>") && system.contains("9337"));
    let status = node.lock().unwrap().status(now_ms());
    assert_eq!(status["counters"]["turns"], 2);
}

#[tokio::test]
async fn opt_out_of_memory_and_reject_bad_requests() {
    let node = shared_node(DarmConfig::default());
    let (response, _) = handle(
        invocation(
            json!({"messages": [{"role": "user", "content": "secret plans"}], "darm": {"remember": false}}),
            vec![candidate("m", "n")],
        ),
        Arc::clone(&node),
        |_| async { Ok(completion("ok")) },
    )
    .await;
    assert_eq!(response.status_code, 200);
    assert!(node.lock().unwrap().memory().is_empty());

    let (response, _) = handle(
        invocation(json!({"messages": []}), vec![candidate("m", "n")]),
        Arc::clone(&node),
        |_| async { Ok(completion("never")) },
    )
    .await;
    assert_eq!(response.status_code, 400);

    let (response, _) = handle(
        invocation(json!({"messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "x"}}]}]}), vec![candidate("text-only", "n")]),
        Arc::clone(&node),
        |_| async { Ok(completion("never")) },
    )
    .await;
    assert_eq!(response.status_code, 503, "no vision-capable candidate");
}

#[tokio::test]
async fn all_routes_failing_returns_last_failure() {
    let node = shared_node(DarmConfig::default());
    let (response, out) = handle(
        invocation(
            json!({"messages": [{"role": "user", "content": "q"}]}),
            vec![candidate("a", "1"), candidate("b", "2")],
        ),
        Arc::clone(&node),
        |_| async {
            Ok(HostInferenceResponse {
                status_code: 503,
                body: json!({"error": {"message": "busy"}}),
                served_by: None,
            })
        },
    )
    .await;
    assert_eq!(response.status_code, 503);
    assert!(out.is_empty());
    assert_eq!(node.lock().unwrap().counters.turns_failed, 1);
}

#[tokio::test]
async fn follow_ups_score_the_memories_an_answer_used() {
    const ANSWER: &str =
        "They are in the ops repository under docs/incident; the platform team maintains them.";
    let node = shared_node(DarmConfig::default());
    let seeded = {
        let mut n = node.lock().unwrap();
        let (id, _) = n
            .remember_exchange(
                "where are the runbooks?",
                "The runbooks live in the ops repository under docs/incident, maintained by the platform team.",
                "m",
                "default",
                now_ms(),
            )
            .unwrap();
        id
    };
    let usefulness = |node: &Arc<Mutex<DarmNode>>| {
        let mut n = node.lock().unwrap();
        let hits = n.recall("where are the runbooks?", "default", 5, now_ms());
        let hit = hits.iter().find(|h| h.id == seeded).expect("seeded memory");
        (hit.usefulness, hit.outcomes)
    };
    let turn = |messages: Value, reply: &'static str| {
        let node = Arc::clone(&node);
        async move {
            let request = json!({"messages": messages, "darm": {"remember": false}});
            let (response, _) = handle(
                invocation(request, vec![candidate("model-a", "n1")]),
                node,
                move |_| async move { Ok(completion(reply)) },
            )
            .await;
            assert_eq!(response.status_code, 200);
        }
    };
    let question = json!({"role": "user", "content": "where can I find the runbooks?"});
    let answer = json!({"role": "assistant", "content": ANSWER});

    // The answer draws on the memory: it waits for the user's reaction.
    turn(json!([question]), ANSWER).await;
    assert_eq!(node.lock().unwrap().auto.waiting(), 1);
    assert_eq!(usefulness(&node).1, 0, "nothing reported yet");

    // The user objects: the memory is scored as having hurt. The reply to
    // the objection ignores the memory, so nothing new waits.
    turn(
        json!([question, answer, {"role": "user", "content": "No, that's wrong, they moved."}]),
        "Sorry about that.",
    )
    .await;
    let (after_hurt, outcomes) = usefulness(&node);
    assert_eq!(outcomes, 1);
    assert!(after_hurt < 0.5, "{after_hurt}");
    assert_eq!(node.lock().unwrap().auto.reported_hurt, 1);
    assert_eq!(node.lock().unwrap().auto.waiting(), 0);

    // Another conversation continues without objection: scored as helped.
    turn(json!([question]), ANSWER).await;
    turn(
        json!([question, answer, {"role": "user", "content": "Thanks. Who is on call?"}]),
        "I do not have that.",
    )
    .await;
    let (after_help, outcomes) = usefulness(&node);
    assert_eq!(outcomes, 2);
    assert!(after_help > after_hurt);
    assert_eq!(node.lock().unwrap().auto.reported_helped, 1);

    // An answer that ignores the memory is never scored.
    turn(json!([question]), "I am not sure.").await;
    assert_eq!(node.lock().unwrap().auto.waiting(), 0);
}

#[tokio::test]
async fn automatic_outcomes_are_off_on_a_public_mesh_unless_configured() {
    let node = shared_node(DarmConfig::default());
    assert!(node.lock().unwrap().auto_outcome_enabled());
    node.lock().unwrap().set_public_mesh(true);
    assert!(!node.lock().unwrap().auto_outcome_enabled());
    let forced = shared_node(DarmConfig {
        auto_outcome: Some(true),
        ..DarmConfig::default()
    });
    forced.lock().unwrap().set_public_mesh(true);
    assert!(forced.lock().unwrap().auto_outcome_enabled());
    let off = shared_node(DarmConfig {
        auto_outcome: Some(false),
        ..DarmConfig::default()
    });
    assert!(!off.lock().unwrap().auto_outcome_enabled());
}
