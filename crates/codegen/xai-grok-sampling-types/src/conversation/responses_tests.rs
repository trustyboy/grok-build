use super::test_support::*;
use super::*;
use crate::tool_overrides::*;
use assert_matches::assert_matches;

#[test]
fn test_conversation_request_to_responses_api() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("System prompt"),
        ConversationItem::user("User message"),
    ])
    .with_model("grok-3")
    .with_temperature(0.7);

    let responses_req: rs::CreateResponse = (&req).into();
    assert_eq!(responses_req.model, Some("grok-3".to_string()));
    assert_eq!(responses_req.temperature, Some(0.7));

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    assert_eq!(items.len(), 2);
}

#[test]
fn function_tool_colliding_with_hosted_web_search_is_dropped() {
    let mut req =
        ConversationRequest::from_items(vec![ConversationItem::user("hi")]).with_tools(vec![
            ToolSpec {
                name: "web_search".to_string(),
                description: Some("local web search".to_string()),
                parameters: serde_json::json!({"type": "object"}),
            },
            ToolSpec {
                name: "read_file".to_string(),
                description: None,
                parameters: serde_json::json!({"type": "object"}),
            },
        ]);
    req.hosted_tools = vec![HostedTool::WebSearch { options: None }];

    let responses_req: rs::CreateResponse = (&req).into();
    let tools = responses_req.tools.expect("tools should be set");

    // web_search is emitted as a raw-JSON `extra_tool_entries` entry, so it never appears as a native `rs::Tool::WebSearch`
    // The raw entry can carry `excluded_domains`, which async_openai's typed filter omits
    let web_search_count = tools
        .iter()
        .filter(|t| matches!(t, rs::Tool::WebSearch(_)))
        .count();
    assert_eq!(
        web_search_count, 0,
        "web_search is not a native tool: {tools:?}"
    );
    let function_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| match t {
            rs::Tool::Function(f) => Some(f.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        function_names,
        vec!["read_file"],
        "colliding function tool must be dropped"
    );

    // The hosted web_search is emitted as a raw entry instead.
    let entries = extra_tool_entries(&req.hosted_tools);
    assert_eq!(entries, vec![serde_json::json!({"type": "web_search"})]);
}

#[test]
fn function_tool_colliding_with_hosted_x_search_is_dropped() {
    let mut req =
        ConversationRequest::from_items(vec![ConversationItem::user("hi")]).with_tools(vec![
            ToolSpec {
                name: "x_search".to_string(),
                description: None,
                parameters: serde_json::json!({"type": "object"}),
            },
        ]);
    req.hosted_tools = vec![HostedTool::XSearch { options: None }];

    let responses_req: rs::CreateResponse = (&req).into();
    let tools = responses_req.tools.unwrap_or_default();
    assert!(tools.is_empty(), "expected no tools, got: {tools:?}");
    let entries = extra_tool_entries(&req.hosted_tools);
    assert_eq!(entries, vec![serde_json::json!({"type": "x_search"})]);
}

/// The hosted `web_search` domain policy only reaches the API through this raw entry (async_openai's typed filters model no blocklist).
/// Both filters must survive the conversion from `HostedTool` to `extra_tool_entries`.
/// An empty or absent policy must stay byte-identical to the bare tool.
#[test]
fn web_search_domain_filters_reach_the_tool_entry() {
    let hosted = |options: Option<WebSearchOptions>| {
        extra_tool_entries(&[HostedTool::WebSearch { options }])
    };
    assert_eq!(
        hosted(Some(WebSearchOptions {
            allowed_domains: Some(vec!["docs.x.ai".into(), "arxiv.org".into()]),
            excluded_domains: None,
        })),
        vec![serde_json::json!({
            "type": "web_search",
            "filters": { "allowed_domains": ["docs.x.ai", "arxiv.org"] },
        })]
    );
    assert_eq!(
        hosted(Some(WebSearchOptions {
            allowed_domains: None,
            excluded_domains: Some(vec!["reddit.com".into()]),
        })),
        vec![serde_json::json!({
            "type": "web_search",
            "filters": { "excluded_domains": ["reddit.com"] },
        })]
    );

    // No policy (absent, default, or empty lists) emits the bare tool.
    let bare = vec![serde_json::json!({ "type": "web_search" })];
    assert_eq!(hosted(None), bare);
    assert_eq!(hosted(Some(WebSearchOptions::default())), bare);
    assert_eq!(
        hosted(Some(WebSearchOptions {
            allowed_domains: Some(vec![]),
            excluded_domains: Some(vec![]),
        })),
        bare
    );
}

#[test]
fn x_search_serializes_to_the_tool_entry() {
    // A full bound reaches the flat snake_case entry; an empty or `None` bound emits the bare entry.
    let dated = extra_tool_entries(&[HostedTool::XSearch {
        options: Some(XSearchOptions {
            date_bound: Some(
                SearchDateBound::new(Some("2024-01-01".into()), Some("2024-03-15".into())).unwrap(),
            ),
        }),
    }]);
    assert_eq!(
        dated,
        vec![serde_json::json!({
            "type": "x_search",
            "from_date": "2024-01-01",
            "to_date": "2024-03-15",
        })]
    );
    let bare = vec![serde_json::json!({"type": "x_search"})];
    assert_eq!(
        extra_tool_entries(&[HostedTool::XSearch {
            options: Some(XSearchOptions {
                date_bound: Some(SearchDateBound::new(None, None).unwrap()),
            }),
        }]),
        bare
    );
    assert_eq!(
        extra_tool_entries(&[HostedTool::XSearch { options: None }]),
        bare
    );
}

#[test]
fn function_web_search_kept_when_no_hosted_tools() {
    let req = ConversationRequest::from_items(vec![ConversationItem::user("hi")]).with_tools(vec![
        ToolSpec {
            name: "web_search".to_string(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
        },
    ]);

    let responses_req: rs::CreateResponse = (&req).into();
    let tools = responses_req.tools.expect("tools should be set");
    let function_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| match t {
            rs::Tool::Function(f) => Some(f.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(function_names, vec!["web_search"]);
}

#[test]
fn test_responses_api_response_to_conversation_item() {
    use crate::rs;

    // Create a Response with text output
    let response = rs::Response {
        background: None,
        billing: None,
        conversation: None,
        created_at: 1234567890,
        completed_at: None,
        error: None,
        id: "resp_123".to_string(),
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        metadata: None,
        model: "grok-3".to_string(),
        object: "response".to_string(),
        output: vec![rs::OutputItem::Message(rs::OutputMessage {
            content: vec![rs::OutputMessageContent::OutputText(
                rs::OutputTextContent {
                    text: "Hello from Responses API!".to_string(),
                    annotations: vec![],
                    logprobs: None,
                },
            )],
            id: "msg_123".to_string(),
            role: rs::AssistantRole::Assistant,
            status: rs::OutputStatus::Completed,
        })],
        parallel_tool_calls: None,
        previous_response_id: None,
        prompt: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: None,
        safety_identifier: None,
        service_tier: None,
        status: rs::Status::Completed,
        temperature: None,
        text: None,
        tool_choice: None,
        tools: None,
        top_logprobs: None,
        top_p: None,
        truncation: None,
        usage: None,
    };

    let items = response_to_conversation_items(response);
    let item = items
        .into_iter()
        .next_back()
        .expect("response produces at least a trailing Assistant");
    assert_eq!(item.text_content(), "Hello from Responses API!");
    let ConversationItem::Assistant(a) = &item else {
        panic!("Expected Assistant item");
    };
    assert_eq!(a.model_id, Some("grok-3".to_string()));
    assert_eq!(
        a.reasoning_effort, None,
        "no reasoning config on the response => no effort recorded"
    );

    // Response with function call
    let response_with_fc = rs::Response {
        background: None,
        billing: None,
        conversation: None,
        created_at: 1234567890,
        completed_at: None,
        error: None,
        id: "resp_456".to_string(),
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        metadata: None,
        model: "grok-3".to_string(),
        object: "response".to_string(),
        output: vec![rs::OutputItem::FunctionCall(rs::FunctionToolCall {
            arguments: r#"{"path": "/bar.txt"}"#.to_string(),
            call_id: "call_789".to_string(),
            name: "read_file".to_string(),
            id: None,
            status: None,
        })],
        parallel_tool_calls: None,
        previous_response_id: None,
        prompt: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: None,
        safety_identifier: None,
        service_tier: None,
        status: rs::Status::Completed,
        temperature: None,
        text: None,
        tool_choice: None,
        tools: None,
        top_logprobs: None,
        top_p: None,
        truncation: None,
        usage: None,
    };

    let items = response_to_conversation_items(response_with_fc);
    let item = items
        .into_iter()
        .next_back()
        .expect("response produces at least a trailing Assistant");
    let ConversationItem::Assistant(a) = &item else {
        panic!("Expected Assistant item");
    };
    let [tc] = a.tool_calls.as_slice() else {
        panic!("expected one tool call: {:?}", a.tool_calls);
    };
    assert_eq!(tc.id.as_ref(), "call_789");
    assert_eq!(tc.name, "read_file");
}

#[test]
fn test_response_reasoning_effort_stamped_on_assistant() {
    use crate::rs;

    let response = rs::Response {
        background: None,
        billing: None,
        conversation: None,
        created_at: 1234567890,
        completed_at: None,
        error: None,
        id: "resp_eff".to_string(),
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        metadata: None,
        model: "grok-3".to_string(),
        object: "response".to_string(),
        output: vec![],
        parallel_tool_calls: None,
        previous_response_id: None,
        prompt: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: Some(rs::Reasoning {
            effort: Some(rs::ReasoningEffort::Xhigh),
            summary: None,
        }),
        safety_identifier: None,
        service_tier: None,
        status: rs::Status::Completed,
        temperature: None,
        text: None,
        tool_choice: None,
        tools: None,
        top_logprobs: None,
        top_p: None,
        truncation: None,
        usage: None,
    };

    let items = response_to_conversation_items(response);
    let ConversationItem::Assistant(a) = items.last().expect("trailing Assistant") else {
        panic!("Expected Assistant item");
    };
    assert_eq!(a.reasoning_effort, Some(crate::ReasoningEffort::Xhigh));

    // Round-trips through the persisted representation.
    let json = serde_json::to_string(&items.last().unwrap()).unwrap();
    assert!(json.contains(r#""reasoning_effort":"xhigh""#), "{json}");
    let back: ConversationItem = serde_json::from_str(&json).unwrap();
    let ConversationItem::Assistant(b) = back else {
        panic!("Expected Assistant item");
    };
    assert_eq!(b.reasoning_effort, Some(crate::ReasoningEffort::Xhigh));
}

#[test]
fn test_tool_calls_to_responses_api() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("System"),
        ConversationItem::user("User"),
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "call_1".into(),
            name: "bash".to_string(),
            arguments: r#"{"command": "ls"}"#.into(),
        }]),
    ]);

    let responses_req: rs::CreateResponse = (&req).into();

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let fc_items: Vec<_> = items
        .iter()
        .filter(|item| matches!(item, rs::InputItem::Item(rs::Item::FunctionCall(_))))
        .collect();

    assert_eq!(fc_items.len(), 1, "Expected exactly one FunctionCall item");

    let Some(rs::InputItem::Item(rs::Item::FunctionCall(fc))) = fc_items.first() else {
        panic!("Expected FunctionCall item: {fc_items:?}");
    };
    assert_eq!(fc.call_id, "call_1");
    assert_eq!(fc.name, "bash");
    assert_eq!(fc.arguments, r#"{"command": "ls"}"#);
}

#[test]
fn test_tool_result_to_responses_api() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("System"),
        ConversationItem::user("User"),
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "call_1".into(),
            name: "bash".to_string(),
            arguments: r#"{"command": "ls"}"#.into(),
        }]),
        ConversationItem::tool_result("call_1", "file1.txt\nfile2.txt\nfile3.txt"),
    ]);

    let responses_req: rs::CreateResponse = (&req).into();

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let fco_items: Vec<_> = items
        .iter()
        .filter(|item| matches!(item, rs::InputItem::Item(rs::Item::FunctionCallOutput(_))))
        .collect();

    assert_eq!(
        fco_items.len(),
        1,
        "Expected exactly one FunctionCallOutput item"
    );

    let Some(rs::InputItem::Item(rs::Item::FunctionCallOutput(fco))) = fco_items.first() else {
        panic!("Expected FunctionCallOutput item: {fco_items:?}");
    };
    assert_eq!(fco.call_id, "call_1");
    let rs::FunctionCallOutput::Text(text) = &fco.output else {
        panic!("Expected Text output");
    };
    assert_eq!(text, "file1.txt\nfile2.txt\nfile3.txt");
}

#[test]
fn test_multiple_tool_results_to_responses_api() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::user("Run these commands"),
        ConversationItem::assistant_tool_calls(vec![
            ToolCall {
                id: "call_1".into(),
                name: "bash".to_string(),
                arguments: r#"{"command": "ls"}"#.into(),
            },
            ToolCall {
                id: "call_2".into(),
                name: "bash".to_string(),
                arguments: r#"{"command": "pwd"}"#.into(),
            },
        ]),
        ConversationItem::tool_result("call_1", "output1"),
        ConversationItem::tool_result("call_2", "output2"),
    ]);

    let responses_req: rs::CreateResponse = (&req).into();

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let fco_items: Vec<_> = items
        .iter()
        .filter_map(|item| {
            if let rs::InputItem::Item(rs::Item::FunctionCallOutput(fco)) = item {
                Some(fco)
            } else {
                None
            }
        })
        .collect();

    let [f0, f1] = fco_items.as_slice() else {
        panic!("expected two function call outputs: {fco_items:?}");
    };
    assert_eq!(f0.call_id, "call_1");
    assert_eq!(f1.call_id, "call_2");
}

#[test]
fn test_responses_api_with_encrypted_reasoning() {
    let response = rs::Response {
        background: None,
        billing: None,
        conversation: None,
        created_at: 1234567890,
        completed_at: None,
        error: None,
        id: "resp_456".to_string(),
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        metadata: None,
        model: "grok-3".to_string(),
        object: "response".to_string(),
        output: vec![
            rs::OutputItem::Reasoning(rs::ReasoningItem {
                id: "reasoning_enc".to_string(),
                summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                    text: "Visible thinking summary".to_string(),
                })],
                content: None,
                encrypted_content: Some("enc_base64_encrypted_reasoning_data_here".to_string()),
                status: Some(rs::OutputStatus::Completed),
            }),
            rs::OutputItem::Message(rs::OutputMessage {
                content: vec![rs::OutputMessageContent::OutputText(
                    rs::OutputTextContent {
                        text: "My response based on reasoning.".to_string(),
                        annotations: vec![],
                        logprobs: None,
                    },
                )],
                id: "msg_456".to_string(),
                role: rs::AssistantRole::Assistant,
                status: rs::OutputStatus::Completed,
            }),
        ],
        parallel_tool_calls: None,
        previous_response_id: None,
        prompt: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: None,
        safety_identifier: None,
        service_tier: None,
        status: rs::Status::Completed,
        temperature: None,
        text: None,
        tool_choice: None,
        tools: None,
        top_logprobs: None,
        top_p: None,
        truncation: None,
        usage: None,
    };

    // Exercise the flat-list path: reasoning lives as a sibling
    let items = response_to_conversation_items(response);
    let assistant_idx = items
        .iter()
        .position(|i| matches!(i, ConversationItem::Assistant(_)))
        .expect("assistant present");
    let Some(ConversationItem::Assistant(a)) = items.get(assistant_idx) else {
        panic!("expected assistant at {assistant_idx}: {items:?}");
    };
    assert_eq!(a.content.as_ref(), "My response based on reasoning.");

    let reasoning_sibling = items
        .iter()
        .find_map(|i| match i {
            ConversationItem::Reasoning(r) => Some(r),
            _ => None,
        })
        .expect("reasoning sibling present");
    // Both the text summary and the encrypted content survive
    assert_eq!(
        reasoning_sibling.summary.first().map(|sp| match sp {
            rs::SummaryPart::SummaryText(t) => t.text.as_str(),
        }),
        Some("Visible thinking summary")
    );
    assert_eq!(
        reasoning_sibling.encrypted_content.as_deref(),
        Some("enc_base64_encrypted_reasoning_data_here")
    );
}

#[test]
fn test_responses_api_with_only_encrypted_reasoning() {
    let response = rs::Response {
        background: None,
        billing: None,
        conversation: None,
        created_at: 1234567890,
        completed_at: None,
        error: None,
        id: "resp_789".to_string(),
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        metadata: None,
        model: "grok-3".to_string(),
        object: "response".to_string(),
        output: vec![
            rs::OutputItem::Reasoning(rs::ReasoningItem {
                id: "reasoning_only_enc".to_string(),
                summary: vec![],
                content: None,
                encrypted_content: Some("enc_only_encrypted_no_visible_summary".to_string()),
                status: Some(rs::OutputStatus::Completed),
            }),
            rs::OutputItem::Message(rs::OutputMessage {
                content: vec![rs::OutputMessageContent::OutputText(
                    rs::OutputTextContent {
                        text: "Response.".to_string(),
                        annotations: vec![],
                        logprobs: None,
                    },
                )],
                id: "msg_789".to_string(),
                role: rs::AssistantRole::Assistant,
                status: rs::OutputStatus::Completed,
            }),
        ],
        parallel_tool_calls: None,
        previous_response_id: None,
        prompt: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: None,
        safety_identifier: None,
        service_tier: None,
        status: rs::Status::Completed,
        temperature: None,
        text: None,
        tool_choice: None,
        tools: None,
        top_logprobs: None,
        top_p: None,
        truncation: None,
        usage: None,
    };

    // Flat-list path: reasoning sibling carries the encrypted blob, empty summary maps to an empty `Vec<SummaryPart>`
    let items = response_to_conversation_items(response);
    let reasoning_sibling = items
        .iter()
        .find_map(|i| match i {
            ConversationItem::Reasoning(r) => Some(r),
            _ => None,
        })
        .expect("reasoning sibling present");
    assert!(reasoning_sibling.summary.is_empty());
    assert_eq!(
        reasoning_sibling.encrypted_content.as_deref(),
        Some("enc_only_encrypted_no_visible_summary")
    );
}

#[test]
fn test_encrypted_reasoning_included_in_responses_api_request() {
    // Encrypted reasoning is crucial for context continuity across turns
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("You are helpful"),
        ConversationItem::user("What is 2+2?"),
        // Previous reasoning and assistant: reasoning is a sibling
        ConversationItem::Reasoning(rs::ReasoningItem {
            id: "r1".to_string(),
            summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                text: "Let me calculate 2+2...".to_string(),
            })],
            content: None,
            encrypted_content: Some("enc_secret_reasoning_chain".to_string()),
            status: None,
        }),
        ConversationItem::Assistant(AssistantItem {
            responses_output: None,
            responses_source: None,
            responses_input_fingerprint: None,
            content: "The answer is 4.".into(),
            tool_calls: vec![],
            model_id: Some("grok-3".to_string()),
            model_fingerprint: None,
            reasoning_effort: None,
        }),
        ConversationItem::user("Now what is 3+3?"),
    ]);

    let responses_req: rs::CreateResponse = (&req).into();

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let reasoning_items: Vec<_> = items
        .iter()
        .filter_map(|item| {
            if let rs::InputItem::Item(rs::Item::Reasoning(r)) = item {
                Some(r)
            } else {
                None
            }
        })
        .collect();

    assert_eq!(
        reasoning_items.len(),
        1,
        "Should have exactly one reasoning item"
    );

    let [reasoning] = reasoning_items.as_slice() else {
        panic!("expected one reasoning item: {reasoning_items:?}");
    };
    assert_eq!(
        reasoning.encrypted_content,
        Some("enc_secret_reasoning_chain".to_string())
    );

    // Verify summary text is included
    let [rs::SummaryPart::SummaryText(summary)] = reasoning.summary.as_slice() else {
        panic!("expected one summary: {:?}", reasoning.summary);
    };
    assert_eq!(summary.text, "Let me calculate 2+2...");
}

#[test]
fn test_only_encrypted_reasoning_included_in_request() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::user("Hello"),
        ConversationItem::Reasoning(rs::ReasoningItem {
            id: String::new(),
            summary: vec![],
            content: None,
            encrypted_content: Some("enc_hidden_thoughts".to_string()),
            status: None,
        }),
        ConversationItem::Assistant(AssistantItem {
            responses_output: None,
            responses_source: None,
            responses_input_fingerprint: None,
            content: "Hi!".into(),
            tool_calls: vec![],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }),
    ]);

    let responses_req: rs::CreateResponse = (&req).into();

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let reasoning_items: Vec<_> = items
        .iter()
        .filter_map(|item| {
            if let rs::InputItem::Item(rs::Item::Reasoning(r)) = item {
                Some(r)
            } else {
                None
            }
        })
        .collect();

    let [reasoning] = reasoning_items.as_slice() else {
        panic!("expected one reasoning item: {reasoning_items:?}");
    };

    assert_eq!(
        reasoning.encrypted_content,
        Some("enc_hidden_thoughts".to_string())
    );

    assert!(reasoning.summary.is_empty());
}

#[test]
fn test_no_reasoning_item_when_no_reasoning() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::user("Hello"),
        ConversationItem::assistant("Hi!"), // No reasoning
    ]);

    let responses_req: rs::CreateResponse = (&req).into();

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let reasoning_items: Vec<_> = items
        .iter()
        .filter(|item| matches!(item, rs::InputItem::Item(rs::Item::Reasoning(_))))
        .collect();

    assert!(reasoning_items.is_empty(), "Should have no reasoning items");
}

#[test]
fn test_conversation_request_with_tools_to_responses_api() {
    let tools = vec![ToolSpec {
        name: "search".to_string(),
        description: Some("Search the codebase".to_string()),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"}
            }
        }),
    }];

    let req = ConversationRequest::from_items(vec![ConversationItem::user("Find TODO comments")])
        .with_tools(tools);

    let responses_req: rs::CreateResponse = (&req).into();
    assert!(responses_req.tools.is_some());
    let tools = responses_req.tools.unwrap();
    assert_eq!(tools.len(), 1);

    let Some(rs::Tool::Function(ft)) = tools.first() else {
        panic!("Expected Function tool: {tools:?}");
    };
    assert_eq!(ft.name, "search");
    assert_eq!(ft.description, Some("Search the codebase".to_string()));
}

#[test]
fn test_tool_choice_to_responses_api() {
    // Test Auto
    let req = ConversationRequest::from_items(vec![ConversationItem::user("test")])
        .with_tool_choice(ConversationToolChoice::Auto);
    let responses_req: rs::CreateResponse = (&req).into();
    assert_matches!(
        responses_req.tool_choice,
        Some(rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::Auto))
    );

    // Test Required
    let req = ConversationRequest::from_items(vec![ConversationItem::user("test")])
        .with_tool_choice(ConversationToolChoice::Required);
    let responses_req: rs::CreateResponse = (&req).into();
    assert_matches!(
        responses_req.tool_choice,
        Some(rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::Required))
    );

    // Test Function
    let req = ConversationRequest::from_items(vec![ConversationItem::user("test")])
        .with_tool_choice(ConversationToolChoice::Function("bash".to_string()));
    let responses_req: rs::CreateResponse = (&req).into();
    let Some(rs::ToolChoiceParam::Function(fc)) = responses_req.tool_choice else {
        panic!("Expected Function tool choice");
    };
    assert_eq!(fc.name, "bash");
}

#[test]
fn test_malformed_tool_arguments_sanitized_in_responses_api() {
    let bad_args = r#"{"file_path": "/testbed/cxx_polynomial/include/emsr/remez.h", "old_string": "", new_string": "x"}"#;

    let tool_call = ToolCall {
        id: "call_bad".into(),
        name: "search_replace".to_string(),
        arguments: bad_args.into(),
    };

    let item = ConversationItem::assistant_tool_calls(vec![tool_call]);
    let req = ConversationRequest {
        items: vec![item],
        ..Default::default()
    };

    let rs_req: crate::rs::CreateResponse = (&req).into();

    // The FunctionCall input item must carry sanitized arguments.
    let crate::rs::InputParam::Items(items) = rs_req.input else {
        panic!("Expected InputParam::Items");
    };
    let fc_args = items.iter().find_map(|inp| {
        if let crate::rs::InputItem::Item(crate::rs::Item::FunctionCall(fc)) = inp {
            Some(fc.arguments.clone())
        } else {
            None
        }
    });

    let fc_args = fc_args.expect("should find a FunctionCall input item");
    assert_eq!(
        fc_args, "{}",
        "malformed arguments must be replaced with {{}} in Responses API path"
    );
}

#[test]
fn test_responses_request_carries_reasoning_effort_nested() {
    for (variant, expected) in [
        (crate::ReasoningEffort::None, "none"),
        (crate::ReasoningEffort::Minimal, "minimal"),
        (crate::ReasoningEffort::Low, "low"),
        (crate::ReasoningEffort::Medium, "medium"),
        (crate::ReasoningEffort::High, "high"),
        (crate::ReasoningEffort::Xhigh, "xhigh"),
        (crate::ReasoningEffort::Max, "max"),
    ] {
        let req = ConversationRequest {
            reasoning_effort: Some(variant),
            ..ConversationRequest::from_items(vec![ConversationItem::user("hi")]).with_model("test")
        };
        let resp: crate::rs::CreateResponse = (&req).into();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            json.pointer("/reasoning/effort").and_then(|v| v.as_str()),
            Some(expected),
            "{variant:?} should serialize as reasoning.effort={expected:?}; got: {json:#}",
        );
    }
}

#[test]
fn test_responses_request_omits_effort_when_unset() {
    let req =
        ConversationRequest::from_items(vec![ConversationItem::user("hi")]).with_model("test");
    let resp: crate::rs::CreateResponse = (&req).into();
    let json = serde_json::to_value(&resp).unwrap();
    assert!(
        json.pointer("/reasoning/effort").is_none(),
        "reasoning.effort must be absent when unset; got: {json:#}",
    );
}

#[test]
fn test_btw_cross_api_responses_no_regressions() {
    let items = btw_prepare_items(btw_mid_turn_conversation());
    let req = ConversationRequest::from_items(items);
    let resp: rs::CreateResponse = (&req).into();
    let json = serde_json::to_value(&resp).unwrap();

    let rs::InputParam::Items(input_items) = &resp.input else {
        panic!("Expected InputParam::Items");
    };

    // Count FunctionCall and FunctionCallOutput items.
    let function_calls: Vec<_> = input_items
        .iter()
        .filter_map(|item| {
            if let rs::InputItem::Item(rs::Item::FunctionCall(fc)) = item {
                Some(fc.call_id.clone())
            } else {
                None
            }
        })
        .collect();
    let function_outputs: Vec<_> = input_items
        .iter()
        .filter_map(|item| {
            if let rs::InputItem::Item(rs::Item::FunctionCallOutput(fco)) = item {
                Some(fco.call_id.clone())
            } else {
                None
            }
        })
        .collect();

    // call_1 must be present as both FunctionCall and FunctionCallOutput.
    assert!(
        function_calls.contains(&"call_1".to_string()),
        "completed FunctionCall call_1 must survive; got calls: {function_calls:?}"
    );
    assert!(
        function_outputs.contains(&"call_1".to_string()),
        "completed FunctionCallOutput call_1 must survive; got outputs: {function_outputs:?}"
    );

    assert!(
        !function_calls.contains(&"call_2".to_string()),
        "orphaned FunctionCall call_2 must be removed"
    );

    // No Reasoning items (reasoning was stripped).
    let has_reasoning = input_items
        .iter()
        .any(|item| matches!(item, rs::InputItem::Item(rs::Item::Reasoning(_))));
    assert!(!has_reasoning, "reasoning items must be stripped");

    assert!(
        json.get("temperature").is_none()
            || json.pointer("/temperature").is_some_and(|v| v.is_null()),
        "temperature must be absent; got: {json:#}",
    );
}

#[test]
fn test_transform_cwd_rewrites_reasoning_sibling() {
    // Reasoning siblings are subject to CWD rewriting via `transform_conversation_cwd` (see the `Reasoning(_)` arm)
    let worktree = "/workspace/.grok/worktrees/project/ab-uuid-a";
    let root = "/workspace/project";

    let mut items = vec![
        ConversationItem::Reasoning(rs::ReasoningItem {
            id: "rs_1".to_string(),
            summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                text: format!("thinking about {worktree}"),
            })],
            content: None,
            encrypted_content: None,
            status: None,
        }),
        ConversationItem::Assistant(AssistantItem {
            responses_output: None,
            responses_source: None,
            responses_input_fingerprint: None,
            content: format!("I edited {worktree}/src/main.rs").into(),
            tool_calls: vec![],
            model_id: Some("grok-3".to_string()),
            model_fingerprint: None,
            reasoning_effort: None,
        }),
    ];

    transform_conversation_cwd(&mut items, worktree, root);

    assert_eq!(
        items.get(1).map(|i| i.text_content()),
        Some(format!("I edited {root}/src/main.rs"))
    );
    let Some(ConversationItem::Reasoning(r)) = items.first() else {
        panic!("expected Reasoning sibling: {items:?}");
    };
    let Some(rs::SummaryPart::SummaryText(t)) = r.summary.first() else {
        panic!("expected summary text: {:?}", r.summary)
    };
    assert!(
        !t.text.contains(worktree),
        "reasoning sibling text should be rewritten"
    );
    assert!(t.text.contains(root));
}

// ── Tool result with images tests ──────────────────────────────────────────

#[test]
fn test_tool_result_with_images_to_responses_api() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("System"),
        ConversationItem::user("Read this image"),
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "call_1".into(),
            name: "read_file".to_string(),
            arguments: r#"{"target_file": "photo.png"}"#.into(),
        }]),
        ConversationItem::tool_result_with_images(
            "call_1",
            "Read image file: photo.png",
            vec![ContentPart::Image {
                url: "data:image/png;base64,iVBOR".into(),
            }],
        ),
    ]);

    let responses_req: rs::CreateResponse = (&req).into();

    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let fco_items: Vec<_> = items
        .iter()
        .filter_map(|item| {
            if let rs::InputItem::Item(rs::Item::FunctionCallOutput(fco)) = item {
                Some(fco)
            } else {
                None
            }
        })
        .collect();

    let [fco] = fco_items.as_slice() else {
        panic!("expected one function call output: {fco_items:?}");
    };
    assert_eq!(fco.call_id, "call_1");

    // Should be Content variant, not Text
    let rs::FunctionCallOutput::Content(parts) = &fco.output else {
        panic!("Expected Content output with images, got Text");
    };
    let [p0, p1] = parts.as_slice() else {
        panic!("Expected text + 1 image: {parts:?}");
    };
    assert!(matches!(p0, rs::InputContent::InputText(t) if t.text == "Read image file: photo.png"));
    assert!(
        matches!(p1, rs::InputContent::InputImage(img) if img.image_url.as_deref() == Some("data:image/png;base64,iVBOR"))
    );
}

#[test]
fn test_tool_result_without_images_stays_text() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::user("Run ls"),
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "call_1".into(),
            name: "bash".to_string(),
            arguments: r#"{"command": "ls"}"#.into(),
        }]),
        ConversationItem::tool_result("call_1", "file1.txt\nfile2.txt"),
    ]);

    let responses_req: rs::CreateResponse = (&req).into();
    let rs::InputParam::Items(items) = responses_req.input else {
        panic!("Expected Items input");
    };
    let fco = items
        .iter()
        .find_map(|item| {
            if let rs::InputItem::Item(rs::Item::FunctionCallOutput(fco)) = item {
                Some(fco)
            } else {
                None
            }
        })
        .unwrap();

    assert!(matches!(&fco.output, rs::FunctionCallOutput::Text(t) if t == "file1.txt\nfile2.txt"));
}

#[test]
fn responses_api_conversion_preserves_model_fingerprint() {
    use std::collections::HashMap;

    let mut metadata = HashMap::new();
    metadata.insert("system_fingerprint".into(), "fp_abc123".into());

    let response = rs::Response {
        background: None,
        billing: None,
        conversation: None,
        created_at: 0,
        completed_at: None,
        error: None,
        id: "resp_test".into(),
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        metadata: Some(metadata),
        model: "grok-4.5".into(),
        object: "response".into(),
        output: vec![rs::OutputItem::Message(rs::OutputMessage {
            content: vec![rs::OutputMessageContent::OutputText(
                rs::OutputTextContent {
                    text: "hello".into(),
                    annotations: vec![],
                    logprobs: None,
                },
            )],
            id: "msg_test".into(),
            role: rs::AssistantRole::Assistant,
            status: rs::OutputStatus::Completed,
        })],
        parallel_tool_calls: None,
        previous_response_id: None,
        prompt: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: None,
        safety_identifier: None,
        service_tier: None,
        status: rs::Status::Completed,
        temperature: None,
        text: None,
        tool_choice: None,
        tools: None,
        top_logprobs: None,
        top_p: None,
        truncation: None,
        usage: None,
    };

    let items = response_to_conversation_items(response);
    let item = items
        .into_iter()
        .next_back()
        .expect("response produces at least a trailing Assistant");
    assert_matches!(item, ConversationItem::Assistant(ref a) => {
        assert_eq!(a.model_fingerprint.as_deref(), Some("fp_abc123"));
        assert_eq!(a.model_id.as_deref(), Some("grok-4.5"));
        assert_eq!(a.content.as_ref(), "hello");
    });
}

#[test]
fn empty_reason_reasoning_only() {
    // A response with a Reasoning sibling but empty Assistant content is classified as ReasoningOnly so the retry logic resamples
    let response = ConversationResponse {
        items: vec![
            ConversationItem::Reasoning(rs::ReasoningItem {
                id: "r1".to_string(),
                summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                    text: "thinking but no text output".to_string(),
                })],
                content: None,
                encrypted_content: None,
                status: None,
            }),
            ConversationItem::Assistant(AssistantItem {
                responses_output: None,
                responses_source: None,
                responses_input_fingerprint: None,
                content: String::new().into(),
                tool_calls: Vec::new(),
                model_id: None,
                model_fingerprint: None,
                reasoning_effort: None,
            }),
        ],
        stop_reason: Some(StopReason::Stop),
        usage: None,
        cost_usd_ticks: None,
        message_chunks_emitted: 0,
        doom_loop_signals: Vec::new(),
        stop_message: None,
        message_id: None,
        raw_stop_reason: None,
        stop_sequence: None,
    };
    assert_eq!(
        response.empty_reason(),
        Some(crate::error::EmptyReason::ReasoningOnly)
    );
    assert!(response.is_empty());
}

#[test]
fn build_responses_input_preserves_multi_turn_ordering() {
    // 4-turn conversation where each assistant turn carries reasoning.
    // The wire-level item order must be
    // [Sys, U1, U2, U3, U4, U5, R, A1, R, A2, ...] which would shift the cache prefix every turn.
    fn r(text: &str) -> ConversationItem {
        ConversationItem::Reasoning(rs::ReasoningItem {
            id: text.to_string(),
            summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                text: text.to_string(),
            })],
            content: None,
            encrypted_content: Some(format!("enc_{text}")),
            status: None,
        })
    }
    let items: Vec<ConversationItem> = vec![
        ConversationItem::system("you are helpful"),
        ConversationItem::user("u1"),
        r("r1"),
        ConversationItem::assistant("a1"),
        ConversationItem::user("u2"),
        r("r2"),
        ConversationItem::assistant("a2"),
        ConversationItem::user("u3"),
        r("r3"),
        ConversationItem::assistant("a3"),
        ConversationItem::user("u4"),
        r("r4"),
        ConversationItem::assistant("a4"),
        ConversationItem::user("u5"),
    ];

    let req = ConversationRequest::from_items(items);
    let input = super::responses::build_responses_input(&req);
    let rs::InputParam::Items(wire_items) = input else {
        panic!("expected Items input");
    };

    // Walk the wire items and verify the expected pattern.
    // Roles per wire item: System, User, Reasoning(role=Assistant), Assistant, User, Reasoning, Assistant, ...
    let kinds: Vec<&'static str> = wire_items
        .iter()
        .map(|w| match w {
            rs::InputItem::EasyMessage(m) => match m.role {
                rs::Role::System => "Sys",
                rs::Role::User => "U",
                rs::Role::Assistant => "A",
                _ => "other",
            },
            rs::InputItem::Item(rs::Item::Reasoning(_)) => "R",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "Sys", "U", "R", "A", "U", "R", "A", "U", "R", "A", "U", "R", "A", "U",
        ],
        "multi-turn ordering must preserve interleaved Reasoning ↔ Assistant per turn"
    );
}

#[test]
fn upgrade_legacy_reasoning_singular_chat_completions_text_only() {
    // Chat-completions has only text (no encrypted, no id).
    let raw = serde_json::json!({
        "type": "assistant",
        "content": "answer",
        "reasoning": {"text": "step-by-step plain reasoning"}
    });
    let mut seen = std::collections::HashSet::new();
    let siblings = upgrade_legacy_reasoning(&raw, &mut seen);
    assert_eq!(siblings.len(), 1);
    let Some(ConversationItem::Reasoning(r)) = siblings.first() else {
        panic!("expected Reasoning sibling: {siblings:?}");
    };
    assert_eq!(r.id, "");
    assert!(r.encrypted_content.is_none());
    let Some(rs::SummaryPart::SummaryText(s)) = r.summary.first() else {
        panic!("expected summary text: {:?}", r.summary)
    };
    assert_eq!(s.text, "step-by-step plain reasoning");
}

#[test]
fn upgrade_legacy_reasoning_v0_chat_request_message_shape() {
    // v0 on disk: top-level role and reasoning_content
    let raw = serde_json::json!({
        "role": "assistant",
        "content": "v0 answer",
        "reasoning_content": "v0-style plain text reasoning"
    });
    let mut seen = std::collections::HashSet::new();
    let siblings = upgrade_legacy_reasoning(&raw, &mut seen);
    assert_eq!(siblings.len(), 1);
    let Some(ConversationItem::Reasoning(r)) = siblings.first() else {
        panic!("expected Reasoning sibling: {siblings:?}");
    };
    let Some(rs::SummaryPart::SummaryText(s)) = r.summary.first() else {
        panic!("expected summary text: {:?}", r.summary)
    };
    assert_eq!(s.text, "v0-style plain text reasoning");
}

#[test]
fn patch_reasoning_text_types_injects_type_discriminator() {
    // Build a request body containing a reasoning item whose nested `content[]` entries lack the `type` field (the async-openai gap)
    let mut body = serde_json::json!({
        "input": [
            {
                "type": "reasoning",
                "id": "r1",
                "content": [
                    { "text": "thinking..." },
                    { "text": "more thinking" }
                ]
            },
            {
                "type": "message",
                "role": "user",
                "content": "hi"
            }
        ]
    });
    patch_reasoning_text_types(&mut body);
    let reasoning_content = body
        .pointer("/input/0/content")
        .and_then(|v| v.as_array())
        .expect("reasoning content array");
    for item in reasoning_content {
        assert_eq!(
            item.get("type").and_then(|t| t.as_str()),
            Some("reasoning_text"),
            "every nested content item must carry the discriminator"
        );
    }
    // Untouched: the user message stays as-is.
    assert_eq!(
        body.pointer("/input/1/content").and_then(|v| v.as_str()),
        Some("hi")
    );
}

#[test]
fn patch_reasoning_text_types_preserves_existing_type() {
    let mut body = serde_json::json!({
        "input": [
            {
                "type": "reasoning",
                "id": "r1",
                "content": [
                    // Post-upstream-fix shape: discriminator already present.
                    { "type": "reasoning_text", "text": "already tagged" },
                    // A hypothetical different discriminator must NOT be clobbered.
                    { "type": "some_future_variant", "text": "future shape" },
                    // Current gap: missing type gets filled in
                    { "text": "needs tag" }
                ]
            }
        ]
    });
    patch_reasoning_text_types(&mut body);
    let content = body
        .pointer("/input/0/content")
        .and_then(|v| v.as_array())
        .expect("reasoning content array");

    // Existing discriminators preserved verbatim (no clobber).
    assert_eq!(
        content
            .first()
            .and_then(|c| c.get("type"))
            .and_then(|t| t.as_str()),
        Some("reasoning_text"),
    );
    assert_eq!(
        content
            .get(1)
            .and_then(|c| c.get("type"))
            .and_then(|t| t.as_str()),
        Some("some_future_variant"),
        "a non-default upstream discriminator must be left untouched",
    );
    // Only the type-less item is filled in.
    assert_eq!(
        content
            .get(2)
            .and_then(|c| c.get("type"))
            .and_then(|t| t.as_str()),
        Some("reasoning_text"),
    );

    // Object integrity: each item has exactly one `type` and its `text`.
    for item in content {
        let obj = item.as_object().expect("content item is an object");
        assert!(obj.contains_key("type") && obj.contains_key("text"));
    }
}

#[test]
fn build_responses_input_single_reasoning_sibling_lands_inline() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("sys"),
        ConversationItem::user("u1"),
        reasoning_sibling("r_abc", "thinking", Some("enc1")),
        ConversationItem::assistant("hi"),
    ]);

    let input = input_items_json(&req);
    let summary = summarise_input(&input);

    // Expected: [system, user, reasoning, assistant]
    let [s0, s1, s2, s3] = summary.as_slice() else {
        panic!("expected four summary items, got: {summary:?}");
    };
    assert_eq!(s0, "system:sys");
    assert_eq!(s1, "user:u1");
    assert_eq!(s2, "reasoning:r_abc");
    assert_eq!(s3, "assistant:hi");

    // No placeholder strings must appear
    let body_str = serde_json::to_string(&input).unwrap();
    assert!(
        !body_str.contains("__RAW_OUTPUT_PLACEHOLDER_"),
        "no placeholder strings post-refactor"
    );

    assert_eq!(
        input
            .get(2)
            .and_then(|v| v.get("encrypted_content"))
            .and_then(|v| v.as_str()),
        Some("enc1"),
    );
}

#[test]
fn build_responses_input_multi_turn_reasoning_ordering() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("sys"),
        ConversationItem::user("u1"),
        reasoning_sibling("r1", "think 1", Some("enc1")),
        ConversationItem::assistant("a1"),
        ConversationItem::tool_result("tc1", "result1"),
        ConversationItem::user("u2"),
        reasoning_sibling("r2", "think 2", Some("enc2")),
        ConversationItem::assistant("a2"),
        ConversationItem::tool_result("tc2", "result2"),
        ConversationItem::user("u3"),
        reasoning_sibling("r3", "think 3", Some("enc3")),
        ConversationItem::assistant("a3"),
    ]);

    let input = input_items_json(&req);
    let summary = summarise_input(&input);

    // INVARIANT 1: There must be exactly N reasoning items for N siblings
    // The pre-refactor bug produced only 1
    let reasoning_count = summary
        .iter()
        .filter(|s| s.starts_with("reasoning:"))
        .count();
    assert_eq!(
        reasoning_count, 3,
        "must have 3 reasoning items, got {reasoning_count}. Items: {summary:?}"
    );

    // INVARIANT 2: Each reasoning must be BETWEEN its corresponding user message and the NEXT user message
    // Without this check, all reasoning items bunched at the end would still pass count
    let user_positions: Vec<usize> = summary
        .iter()
        .enumerate()
        .filter(|(_, s)| s.starts_with("user:"))
        .map(|(i, _)| i)
        .collect();
    let reasoning_positions: Vec<usize> = summary
        .iter()
        .enumerate()
        .filter(|(_, s)| s.starts_with("reasoning:"))
        .map(|(i, _)| i)
        .collect();

    assert_eq!(user_positions.len(), 3);
    assert_eq!(reasoning_positions.len(), 3);

    for (i, rp) in reasoning_positions.iter().enumerate() {
        let Some(user_pos) = user_positions.get(i).copied() else {
            panic!("missing user position {i}: {user_positions:?}");
        };
        assert!(
            *rp > user_pos,
            "reasoning {i} at position {rp} must be after user {i} at position {user_pos}. \
                 Items: {summary:?}",
        );
        if let Some(next_user) = user_positions.get(i + 1).copied() {
            assert!(
                *rp < next_user,
                "reasoning {i} at position {rp} must be before user {} at position {next_user}. \
                     Items: {summary:?}",
                i + 1,
            );
        }
    }

    // INVARIANT 3: encrypted_content per item is preserved 1:1.
    let mut enc_seen: Vec<&str> = Vec::new();
    for v in &input {
        if v.get("type").and_then(|t| t.as_str()) == Some("reasoning")
            && let Some(enc) = v.get("encrypted_content").and_then(|s| s.as_str())
        {
            enc_seen.push(enc);
        }
    }
    assert_eq!(enc_seen, vec!["enc1", "enc2", "enc3"]);
}

#[test]
fn backend_tool_call_position_stable() {
    let ws_a = ConversationItem::BackendToolCall(BackendToolCallItem {
        kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
            id: "ws_a".to_string(),
            status: rs::WebSearchToolCallStatus::Completed,
            action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                query: "alpha".to_string(),
                sources: Some(vec![]),
            }),
        }),
    });
    let ws_b = ConversationItem::BackendToolCall(BackendToolCallItem {
        kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
            id: "ws_b".to_string(),
            status: rs::WebSearchToolCallStatus::Completed,
            action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                query: "beta".to_string(),
                sources: Some(vec![]),
            }),
        }),
    });

    let req = ConversationRequest::from_items(vec![
        ConversationItem::user("u1"),
        reasoning_sibling("r1", "think a", Some("enc_a")),
        ws_a,
        ConversationItem::assistant("a1"),
        ConversationItem::user("u2"),
        ws_b,
        ConversationItem::assistant("a2"),
    ]);

    let input = input_items_json(&req);
    let ws_items: Vec<&serde_json::Value> = input
        .iter()
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("web_search_call"))
        .collect();

    // Both backend tool calls must survive serialization.
    assert_eq!(
        ws_items.len(),
        2,
        "both web_search_call items must survive; got: {:?}",
        summarise_input(&input)
    );
    let ids: Vec<&str> = ws_items
        .iter()
        .filter_map(|v| v.get("id").and_then(|i| i.as_str()))
        .collect();
    assert_eq!(ids, vec!["ws_a", "ws_b"], "ordering preserved");
}

#[test]
fn empty_content_assistant_with_tool_calls_and_reasoning() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::user("u1"),
        reasoning_sibling("r1", "must call a tool", Some("enc_pre_tool")),
        ConversationItem::Assistant(AssistantItem {
            responses_output: None,
            responses_source: None,
            responses_input_fingerprint: None,
            content: Arc::<str>::from(""),
            tool_calls: vec![ToolCall {
                id: Arc::<str>::from("call_1"),
                name: "read_file".to_string(),
                arguments: Arc::<str>::from("{}"),
            }],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }),
        ConversationItem::tool_result("call_1", "file contents"),
    ]);

    let input = input_items_json(&req);
    let summary = summarise_input(&input);

    // (assistant message DROPPED because content is empty -- per conversation_item_to_input_items, lines 1718-1724) function_call_output (tool result)
    // No spurious extra reasoning items, no placeholder.
    let reasoning_count = summary
        .iter()
        .filter(|s| s.starts_with("reasoning:"))
        .count();
    assert_eq!(
        reasoning_count, 1,
        "exactly one reasoning item; got: {summary:?}"
    );
    assert!(
        summary.iter().any(|s| s == "function_call:call_1"),
        "function_call must appear; got: {summary:?}"
    );
    assert!(
        summary
            .iter()
            .any(|s| s.starts_with("type:function_call_output")),
        "function_call_output must appear; got: {summary:?}"
    );

    let body_str = serde_json::to_string(&input).unwrap();
    assert!(!body_str.contains("__RAW_OUTPUT_PLACEHOLDER_"));
}

#[test]
fn serialized_body_contains_no_placeholder_strings() {
    let req = ConversationRequest::from_items(vec![
        ConversationItem::system("sys"),
        ConversationItem::user("u1"),
        reasoning_sibling("r1", "first", Some("enc1")),
        ConversationItem::assistant("a1"),
        ConversationItem::user("u2"),
        reasoning_sibling("r2", "second", Some("enc2")),
        ConversationItem::assistant("a2"),
        ConversationItem::user("u3"),
    ]);

    let cr: rs::CreateResponse = (&req).into();
    let mut body = serde_json::to_value(&cr).unwrap();
    patch_reasoning_text_types(&mut body);
    let body_str = serde_json::to_string(&body).unwrap();

    assert!(
        !body_str.contains("__RAW_OUTPUT_PLACEHOLDER_"),
        "no placeholder strings post-sibling-Reasoning refactor"
    );

    // Both reasoning items must appear inline in the input array.
    let Some(input) = body.get("input").and_then(|v| v.as_array()) else {
        panic!("expected input array: {body:?}");
    };
    let reasoning_items: Vec<&serde_json::Value> = input
        .iter()
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("reasoning"))
        .collect();
    assert_eq!(
        reasoning_items.len(),
        2,
        "both reasoning siblings must be present"
    );
}

/// 最小响应保留原始输出通道，typed output 可为空。
fn raw_history_response(output: serde_json::Value) -> rs::Response {
    let mut response: rs::Response = serde_json::from_value(serde_json::json!({
        "id":"resp_raw", "object":"response", "created_at":1,
        "model":"test", "status":"completed", "output":[]
    }))
    .expect("response fixture");
    // 模拟适配器：可识别输出仍进入 typed 通道，原始通道保留完整数据。
    response.output = output
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| serde_json::from_value(item.clone()).ok())
        .collect();
    response.metadata = Some(std::collections::HashMap::from([(
        RESPONSE_RAW_OUTPUT_METADATA_KEY.to_owned(),
        output.to_string(),
    )]));
    response
}

/// 使用真实请求序列化路径验证补丁。
fn patched_history(items: Vec<ConversationItem>) -> serde_json::Value {
    let req = ConversationRequest::from_items(items);
    let mut body = serde_json::to_value(rs::CreateResponse::from(&req)).unwrap();
    patch_responses_input(&mut body, &req);
    body
}

#[test]
fn responses_raw_history_preserves_boundaries_refusal_and_function_identity() {
    let raw = serde_json::json!([
        {"type":"message","id":"m1","role":"assistant","status":"completed",
         "phase":"commentary","content":[{"type":"output_text","text":"checking","annotations":[],"logprobs":[]}]},
        {"type":"function_call","call_id":"c1","name":"read_file","arguments":"{}",
         "namespace":"fs","caller":{"type":"direct"},"status":"completed","extra_output":true},
        {"type":"message","id":"m2","role":"assistant","status":"completed",
         "phase":"final_answer","content":[{"type":"refusal","refusal":"cannot comply"}]}
    ]);
    let items = response_to_conversation_items(raw_history_response(raw.clone()));
    let ConversationItem::Assistant(assistant) = items.last().unwrap() else {
        panic!()
    };
    assert_eq!(assistant.content.as_ref(), "checking\ncannot comply");
    assert_eq!(
        serde_json::to_value(&assistant.responses_output).unwrap(),
        raw
    );
    let persisted = serde_json::to_string(&items).unwrap();
    let restored: Vec<ConversationItem> = serde_json::from_str(&persisted).unwrap();
    let body = patched_history(restored);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert_eq!(body["input"][0]["phase"], "commentary");
    assert_eq!(body["input"][1]["namespace"], "fs");
    assert_eq!(body["input"][1]["caller"]["type"], "direct");
    assert!(body["input"][1].get("status").is_none());
    assert!(body["input"][1].get("extra_output").is_none());
    assert!(body["input"][0]["content"][0].get("logprobs").is_none());
    assert_eq!(body["input"][2]["content"][0]["refusal"], "cannot comply");
    let messages = conversation_to_chat_messages(items);
    assert!(
        serde_json::to_string(&messages)
            .unwrap()
            .contains("cannot comply")
    );
}

#[test]
fn responses_raw_history_filters_unknown_and_client_tools_but_retains_storage() {
    let raw = serde_json::json!([
        {"type":"future_output","payload":{"opaque":true}},
        {"type":"computer_call","id":"computer1","action":{"type":"click"}},
        {"type":"custom_tool_call","id":"custom1","name":"execute","input":"danger"},
        {"type":"compaction","id":"compact1","encrypted_content":"opaque","status":"completed"},
        {"type":"mcp_call","id":"mcp1","name":"lookup","arguments":"{}","server_label":"docs","output":"found","error":null}
    ]);
    let items = response_to_conversation_items(raw_history_response(raw.clone()));
    let ConversationItem::Assistant(assistant) = items.last().unwrap() else {
        panic!()
    };
    assert!(assistant.tool_calls.is_empty());
    assert_eq!(assistant.responses_output.as_ref().unwrap().len(), 5);
    let body = patched_history(items);
    assert_eq!(body["input"].as_array().unwrap().len(), 1);
    assert_eq!(body["input"][0]["type"], "mcp_call");
}

#[test]
fn responses_raw_history_deduplicates_siblings_and_keeps_old_history() {
    let reasoning =
        serde_json::json!({"type":"reasoning","id":"r1","summary":[],"encrypted_content":"secret"});
    let mut items = vec![ConversationItem::assistant("old")];
    items.extend(response_to_conversation_items(raw_history_response(
        serde_json::json!([
            {"type":"message","id":"m1","role":"assistant","status":"completed",
             "content":[{"type":"output_text","text":"before","annotations":[]}]},
            reasoning,
            {"type":"message","id":"m2","role":"assistant","status":"completed",
             "content":[{"type":"output_text","text":"after","annotations":[]}]}
        ]),
    )));
    let body = patched_history(items);
    assert_eq!(body["input"].as_array().unwrap().len(), 4);
    assert_eq!(body["input"][0]["content"], "old");
    assert_eq!(body["input"][2]["type"], "reasoning");
    let old: AssistantItem =
        serde_json::from_value(serde_json::json!({"content":"legacy"})).unwrap();
    assert!(old.responses_output.is_none());
}

#[test]
fn responses_typed_refusal_without_raw_metadata_is_visible() {
    let mut response = raw_history_response(serde_json::json!([]));
    response.metadata = None;
    response.output = serde_json::from_value(serde_json::json!([
        {"type":"message","id":"refusal1","role":"assistant","status":"completed",
         "content":[{"type":"refusal","refusal":"not allowed"}]}
    ]))
    .unwrap();
    let items = response_to_conversation_items(response);
    assert_eq!(items.last().unwrap().text_content(), "not allowed");
    let body = patched_history(items);
    assert_eq!(body["input"][0]["content"][0]["refusal"], "not allowed");
}

#[test]
fn responses_empty_raw_output_keeps_streaming_fallback() {
    let mut items = response_to_conversation_items(raw_history_response(serde_json::json!([])));
    let Some(ConversationItem::Assistant(assistant)) = items.last_mut() else {
        panic!()
    };
    assistant.content = "streamed text".into();
    let body = patched_history(items);
    assert_eq!(body["input"][0]["content"], "streamed text");
}

#[test]
fn responses_x_search_history_supports_all_hosted_names() {
    for name in [
        "x_search",
        "x_keyword_search",
        "x_semantic_search",
        "x_user_search",
        "x_thread_fetch",
    ] {
        let raw = serde_json::json!([{"type":"custom_tool_call", "id":"xs1", "call_id":"xc1", "name":name, "input":"query"}]);
        let items = response_to_conversation_items(raw_history_response(raw));
        assert!(
            matches!(items.first(), Some(ConversationItem::BackendToolCall(_))),
            "{name}"
        );
        let body = patched_history(items.clone());
        assert_eq!(body["input"].as_array().unwrap().len(), 1, "{name}");
        assert_eq!(body["input"][0]["name"], name);
        let legacy: Vec<_> = items
            .into_iter()
            .map(|mut item| {
                if let ConversationItem::Assistant(a) = &mut item {
                    a.responses_output = None;
                }
                item
            })
            .collect();
        assert_eq!(patched_history(legacy)["input"][0]["name"], name);
    }
}

#[test]
fn responses_modified_history_never_replays_stale_snapshot() {
    let raw = serde_json::json!([
        {"type":"message","id":"m1","role":"assistant","status":"completed",
         "content":[{"type":"output_text","text":"original long text","annotations":[]}]},
        {"type":"function_call","call_id":"c1","name":"read_file","arguments":"{}"},
        {"type":"compaction","id":"compact1","encrypted_content":"stale secret"}
    ]);
    for change in ["text", "clear", "arguments", "id"] {
        let mut items = response_to_conversation_items(raw_history_response(raw.clone()));
        let Some(ConversationItem::Assistant(a)) = items.last_mut() else {
            panic!()
        };
        match change {
            "text" => a.content = "short".into(),
            "clear" => a.tool_calls.clear(),
            "arguments" => a.tool_calls.first_mut().unwrap().arguments = "{\"new\":true}".into(),
            "id" => a.tool_calls.first_mut().unwrap().id = "repaired".into(),
            _ => unreachable!(),
        }
        repair_dangling_tool_calls(&mut items, DanglingToolCallReason::UserCancelled);
        let body = patched_history(items);
        let serialized = body.to_string();
        assert!(!serialized.contains("stale secret"), "{change}");
        if change == "text" {
            assert!(!serialized.contains("original long text"));
        }
        if change == "clear" {
            assert!(!serialized.contains("function_call"));
        }
        if change == "id" {
            assert!(!serialized.contains("\"c1\""));
        }
    }
}

#[test]
fn responses_removed_or_changed_siblings_are_not_restored() {
    let raw = serde_json::json!([
        {"type":"reasoning","id":"r1","summary":[{"type":"summary_text","text":"old reasoning"}],"encrypted_content":"old cipher"},
        {"type":"custom_tool_call","id":"xs1","call_id":"xc1","name":"x_search","input":"query"},
        {"type":"message","id":"m1","role":"assistant","status":"completed",
         "content":[{"type":"output_text","text":"answer","annotations":[]}]}
    ]);
    let mut items = response_to_conversation_items(raw_history_response(raw));
    items.retain(|item| !matches!(item, ConversationItem::BackendToolCall(_)));
    let Some(ConversationItem::Reasoning(reasoning)) = items.first_mut() else {
        panic!()
    };
    reasoning.encrypted_content = None;
    reasoning.summary.clear();
    let body = patched_history(items.clone());
    assert!(!body.to_string().contains("old cipher"));
    assert!(!body.to_string().contains("old reasoning"));
    assert!(!body.to_string().contains("custom_tool_call"));
    items.retain(|item| !matches!(item, ConversationItem::Reasoning(_)));
    assert!(
        !patched_history(items)["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.get("type").and_then(|value| value.as_str()) == Some("reasoning"))
    );
}

#[test]
fn responses_mcp_and_compaction_are_sdk_input_items() {
    let raw = serde_json::json!([
        {"type":"mcp_call","id":"mc1","name":"lookup","arguments":"{}","server_label":"docs","output":"found","output_only":true},
        {"type":"mcp_list_tools","id":"ml1","server_label":"docs","tools":[],"output_only":true},
        {"type":"mcp_approval_request","id":"ma1","name":"lookup","arguments":"{}","server_label":"docs","output_only":true},
        {"type":"compaction","id":"co1","encrypted_content":"opaque","output_only":true}
    ]);
    for item in raw.as_array().unwrap() {
        serde_json::from_value::<rs::Item>(item.clone())
            .expect("fixed SDK supports this input type");
    }
    let body = patched_history(response_to_conversation_items(raw_history_response(raw)));
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert!(
        input
            .iter()
            .all(|item| item.get("type").and_then(|value| value.as_str()) != Some("compaction"))
    );
    for item in input {
        assert!(item.get("output_only").is_none());
        serde_json::from_value::<rs::Item>(item.clone())
            .expect("replayed item remains legal SDK input");
    }
}

/// 使用原 v1 算法作为持久化兼容性基准。
fn legacy_responses_history_fingerprint(items: &[ConversationItem], source: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(serde_json::to_vec(&("responses-history-v1", source, items)).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn responses_history_streaming_fingerprint_matches_v1_at_every_prefix() {
    let mut items = vec![
        ConversationItem::system("中文 🦀\n\t\"quoted\"\\path\u{0000}"),
        ConversationItem::User(UserItem {
            content: vec![
                ContentPart::Text {
                    text: Arc::from("多模态\r\n\\\""),
                },
                ContentPart::Image {
                    url: Arc::from(format!("data:image/png;base64,{}", "aB09".repeat(1_024))),
                },
            ],
            synthetic_reason: SyntheticReason::AgentMessage,
            cwd_generation: Some(7),
            ..UserItem::default()
        }),
    ];
    items.extend(response_to_conversation_items(raw_history_response(
        serde_json::json!([
            {"type":"reasoning", "id":"r1", "summary":[{"type":"summary_text", "text":"推理\n\\\""}], "encrypted_content":"密文🦀"},
            {"type":"web_search_call", "id":"ws1", "status":"completed", "action":{"type":"search", "query":"中文搜索"}},
            {"type":"reasoning", "id":"r2", "summary":[], "encrypted_content":"second"},
            {"type":"compaction", "encrypted_content":"opaque", "metadata":{"unicode":"中文", "escaped":"\n\\\"", "nested":[null, true, 42]}},
            {"type":"message", "id":"m1", "role":"assistant", "content":[{"type":"output_text", "text":"答案🦀\n", "annotations":[]}]}
        ]),
    )));
    let Some(ConversationItem::Assistant(assistant)) = items.last_mut() else {
        panic!("expected assistant");
    };
    assistant.responses_source = Some("原始来源".into());
    assistant.responses_input_fingerprint = Some("persisted fingerprint".into());
    assistant.model_fingerprint = Some("模型指纹".into());
    assistant.reasoning_effort = Some(crate::ReasoningEffort::High);
    items.push(ConversationItem::assistant_tool_calls(vec![ToolCall {
        id: Arc::from("c1"),
        name: "read_file".into(),
        arguments: Arc::from(r#"{"path":"中文\\file\n\""}"#),
    }]));
    items.push(ConversationItem::tool_result("c1", "结果\t🦀"));

    for source in ["", "source", "来源🦀\n\"\\"] {
        for end in 0..=items.len() {
            let req = ConversationRequest::from_items(items[..end].to_vec());
            let expected = legacy_responses_history_fingerprint(&req.items, source);
            let expected_bytes = serde_json::to_vec(&("responses-history-v1", source, &req.items))
                .unwrap()
                .len();
            super::responses::take_responses_history_hashed_bytes();
            assert_eq!(
                responses_input_fingerprint(&req, source),
                expected,
                "prefix {end}"
            );
            assert_eq!(
                super::responses::take_responses_history_hashed_bytes(),
                expected_bytes
            );
        }
    }
}

#[test]
fn responses_history_incremental_prefixes_match_v1_with_linear_hash_bytes() {
    let mut req = ConversationRequest::from_items(Vec::new());
    let mut checkpoints = Vec::new();
    for turn in 0..8 {
        if turn > 0 {
            req.items
                .push(ConversationItem::user("追加🦀\n\\\"".repeat(128)));
        }
        let source = if turn % 3 == 1 { "other" } else { "source" };
        let prefix_end = req.items.len();
        let cipher = format!("checkpoint-{turn}");
        let mut response = raw_history_response(serde_json::json!([
            {"type":"reasoning", "id":format!("r-{turn}"), "summary":[], "encrypted_content":"推理🦀"},
            {"type":"web_search_call", "id":format!("ws-{turn}"), "status":"completed", "action":{"type":"search", "query":"搜索\n\\\""}},
            {"type":"reasoning", "id":format!("r-after-{turn}"), "summary":[], "encrypted_content":"尾部推理"},
            {"type":"compaction", "encrypted_content":cipher},
            {"type":"message", "id":format!("m-{turn}"), "role":"assistant", "content":[{"type":"output_text", "text":"答案🦀", "annotations":[]}], "metadata":{"turn":turn, "escaped":"\n\\\""}}
        ]));
        let metadata = response.metadata.as_mut().unwrap();
        metadata.insert(RESPONSE_SOURCE_METADATA_KEY.into(), source.into());
        metadata.insert(
            RESPONSE_INPUT_FINGERPRINT_METADATA_KEY.into(),
            legacy_responses_history_fingerprint(&req.items, source),
        );
        req.items.extend(response_to_conversation_items(response));
        checkpoints.push((source, prefix_end, cipher));

        for selected_source in ["source", "other"] {
            let matching: Vec<_> = checkpoints
                .iter()
                .filter(|(source, _, _)| *source == selected_source)
                .collect();
            super::responses::take_responses_history_hashed_bytes();
            let projection = project_responses_input(&req, Some(selected_source));
            let hashed_bytes = super::responses::take_responses_history_hashed_bytes();
            if let Some((_, end, expected_cipher)) = matching.last() {
                let full_prefix_bytes = serde_json::to_vec(&(
                    "responses-history-v1",
                    selected_source,
                    &req.items[..*end],
                ))
                .unwrap()
                .len();
                // 完整前缀只写入一次，每个快照仅额外写入两个闭合括号。
                assert_eq!(hashed_bytes, full_prefix_bytes - 2 + matching.len() * 2);
                assert_eq!(
                    projection.input[0]["encrypted_content"],
                    expected_cipher.as_str()
                );
                assert!(projection.checkpoint_key.is_some());
            } else {
                assert_eq!(hashed_bytes, 0);
                assert!(projection.checkpoint_key.is_none());
            }
        }
    }
}

#[test]
fn responses_history_skips_hashing_without_a_source_candidate() {
    let mut req = ConversationRequest::from_items(vec![ConversationItem::user("history")]);
    append_checkpoint(
        &mut req,
        serde_json::json!([{"type":"compaction", "encrypted_content":"opaque"}]),
        "source",
    );
    for source in [None, Some(""), Some("other")] {
        super::responses::take_responses_history_hashed_bytes();
        assert!(
            project_responses_input(&req, source)
                .checkpoint_key
                .is_none()
        );
        assert_eq!(super::responses::take_responses_history_hashed_bytes(), 0);
    }
    super::responses::take_responses_history_hashed_bytes();
    let mut body = serde_json::json!({});
    patch_responses_input(&mut body, &req);
    assert_eq!(super::responses::take_responses_history_hashed_bytes(), 0);

    let Some(ConversationItem::Assistant(assistant)) = req.items.last_mut() else {
        panic!("expected assistant");
    };
    assistant.responses_input_fingerprint = None;
    assert!(
        project_responses_input(&req, Some("source"))
            .checkpoint_key
            .is_none()
    );
    assert_eq!(super::responses::take_responses_history_hashed_bytes(), 0);
}

/// 模拟客户端使用发送前的本地历史签发压缩来源。
fn append_checkpoint(req: &mut ConversationRequest, raw: serde_json::Value, source: &str) {
    let mut response = raw_history_response(raw);
    let metadata = response.metadata.as_mut().unwrap();
    metadata.insert(RESPONSE_SOURCE_METADATA_KEY.into(), source.into());
    metadata.insert(
        RESPONSE_INPUT_FINGERPRINT_METADATA_KEY.into(),
        responses_input_fingerprint(req, source),
    );
    req.items.extend(response_to_conversation_items(response));
}

/// 原生压缩入口不依赖 SDK 是否支持 compaction 类型。
fn source_history(req: &ConversationRequest, source: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({});
    patch_responses_input_for_source(&mut body, req, source);
    body["input"].clone()
}

#[test]
fn responses_checkpoint_latest_boundary_and_system_instructions() {
    let mut req = ConversationRequest::from_items(vec![
        ConversationItem::system("sys"),
        ConversationItem::user("old"),
    ]);
    append_checkpoint(
        &mut req,
        serde_json::json!([
            {"type":"message","id":"before","role":"assistant","content":[{"type":"output_text","text":"before","annotations":[]}]},
            {"type":"compaction","encrypted_content":"first"},
            {"type":"message","id":"after","role":"assistant","content":[{"type":"output_text","text":"after","annotations":[]}]}
        ]),
        "source",
    );
    let input = source_history(&req, Some("source"));
    assert_eq!(input[0]["content"], "sys");
    assert_eq!(input[1]["encrypted_content"], "first");
    assert_eq!(input[2]["id"], "after");
    req.items.push(ConversationItem::system("new instruction"));
    req.items.push(ConversationItem::user("next"));
    append_checkpoint(
        &mut req,
        serde_json::json!([
            {"type":"compaction","encrypted_content":"second"}
        ]),
        "source",
    );
    let input = source_history(&req, Some("source"));
    assert_eq!(input.as_array().unwrap().len(), 3);
    assert_eq!(input[1]["content"], "new instruction");
    assert_eq!(input[2]["encrypted_content"], "second");
}

#[test]
fn responses_checkpoint_diagnostics_explain_fallback_without_content() {
    let mut req = ConversationRequest::from_items(vec![ConversationItem::user("private")]);
    append_checkpoint(
        &mut req,
        serde_json::json!([
            {"type":"compaction","encrypted_content":"opaque"}
        ]),
        "source",
    );
    let valid = project_responses_input(&req, Some("source"));
    assert_eq!(valid.compaction_candidates, 1);
    assert!(valid.checkpoint_key.is_some());
    assert!(valid.compaction_rejections.is_empty());
    let foreign = project_responses_input(&req, Some("other"));
    assert_eq!(
        foreign.compaction_rejections.get("source_changed"),
        Some(&1)
    );
    req.items[0] = ConversationItem::user("edited");
    let edited = project_responses_input(&req, Some("source"));
    assert_eq!(
        edited.compaction_rejections.get("history_changed"),
        Some(&1)
    );
    assert!(edited.checkpoint_key.is_none());
}

#[test]
fn responses_checkpoint_history_edits_and_source_fail_closed() {
    let mut req = ConversationRequest::from_items(vec![ConversationItem::user("private")]);
    append_checkpoint(
        &mut req,
        serde_json::json!([
            {"type":"compaction","encrypted_content":"opaque"}
        ]),
        "source",
    );
    for source in [None, Some("other")] {
        assert!(!source_history(&req, source).to_string().contains("opaque"));
    }
    let mut edited = req.clone();
    edited.items[0] = ConversationItem::user("edited");
    assert!(
        !source_history(&edited, Some("source"))
            .to_string()
            .contains("opaque")
    );
    req.items.remove(0);
    assert!(
        !source_history(&req, Some("source"))
            .to_string()
            .contains("opaque")
    );
}

#[test]
fn responses_checkpoint_tool_boundary_must_be_paired() {
    let mut req = ConversationRequest::from_items(vec![ConversationItem::user("task")]);
    append_checkpoint(
        &mut req,
        serde_json::json!([
            {"type":"function_call","call_id":"c1","name":"read_file","arguments":"{}"},
            {"type":"compaction","encrypted_content":"unsafe"}
        ]),
        "source",
    );
    req.items
        .push(ConversationItem::tool_result("c1", "result"));
    let input = source_history(&req, Some("source"));
    assert!(!input.to_string().contains("unsafe"));
    assert!(input.to_string().contains("function_call_output"));
}

#[test]
fn responses_checkpoint_incomplete_response_and_legacy_are_not_trusted() {
    for status in ["incomplete", "failed", "cancelled", "in_progress"] {
        let mut response = raw_history_response(serde_json::json!([]));
        response.status = serde_json::from_value(serde_json::json!(status)).unwrap();
        response
            .metadata
            .as_mut()
            .unwrap()
            .insert(RESPONSE_SOURCE_METADATA_KEY.into(), "source".into());
        let items = response_to_conversation_items(response);
        let ConversationItem::Assistant(assistant) = items.last().unwrap() else {
            panic!()
        };
        assert!(assistant.responses_source.is_none());
        assert!(assistant.responses_input_fingerprint.is_none());
    }
    let old: AssistantItem =
        serde_json::from_value(serde_json::json!({"content":"legacy"})).unwrap();
    assert!(old.responses_source.is_none());
    assert!(old.responses_input_fingerprint.is_none());
}

#[test]
fn responses_checkpoint_image_removal_and_sibling_edits_invalidate() {
    let image_user: ConversationItem = serde_json::from_value(serde_json::json!({
        "type":"user", "content":[{"type":"text","text":"look"},
        {"type":"image","url":"data:image/png;base64,old"}]
    }))
    .unwrap();
    let mut req = ConversationRequest::from_items(vec![image_user]);
    append_checkpoint(
        &mut req,
        serde_json::json!([
            {"type":"reasoning","id":"r1","summary":[],"encrypted_content":"reason"},
            {"type":"compaction","encrypted_content":"opaque"}
        ]),
        "source",
    );
    assert!(
        source_history(&req, Some("source"))
            .to_string()
            .contains("opaque")
    );
    let mut edited = req.clone();
    if let ConversationItem::User(user) = &mut edited.items[0] {
        user.content.pop();
    }
    assert!(
        !source_history(&edited, Some("source"))
            .to_string()
            .contains("opaque")
    );
    req.items
        .retain(|item| !matches!(item, ConversationItem::Reasoning(_)));
    assert!(
        !source_history(&req, Some("source"))
            .to_string()
            .contains("opaque")
    );
}

#[test]
fn responses_checkpoint_keeps_paired_tools_after_boundary() {
    let mut req = ConversationRequest::from_items(vec![ConversationItem::user("task")]);
    append_checkpoint(
        &mut req,
        serde_json::json!([
            {"type":"compaction","encrypted_content":"safe"},
            {"type":"function_call","call_id":"c1","name":"read_file","arguments":"{}"}
        ]),
        "source",
    );
    req.items
        .push(ConversationItem::tool_result("c1", "result"));
    let input = source_history(&req, Some("source"));
    assert_eq!(input.as_array().unwrap().len(), 3);
    assert_eq!(input[0]["encrypted_content"], "safe");
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[2]["type"], "function_call_output");
}

#[test]
fn responses_checkpoint_prepared_history_stays_stable_across_turns() {
    let mut persisted = vec![
        ConversationItem::system("sys\n\n<memory_context>stable</memory_context>"),
        ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::from("agent message"),
            }],
            synthetic_reason: SyntheticReason::AgentMessage,
            ..UserItem::default()
        }),
    ];
    // 对应 ModelRequestHistory：始终从未投影的持久历史克隆，不重复叠加标签。
    let prepare = |items: &[ConversationItem]| {
        let mut items = items.to_vec();
        for item in &mut items {
            if let ConversationItem::User(user) = item
                && user.synthetic_reason == SyntheticReason::AgentMessage
            {
                user.content.insert(
                    0,
                    ContentPart::Text {
                        text: Arc::from(
                            "[Message authored by another agent; not a human request or approval.]",
                        ),
                    },
                );
            }
        }
        ConversationRequest::from_items(items)
    };
    for turn in 0..3 {
        let mut request = prepare(&persisted);
        let input_count = request.items.len();
        append_checkpoint(
            &mut request,
            serde_json::json!([
                {"type":"compaction","encrypted_content":format!("checkpoint-{turn}")},
                {"type":"message","id":format!("m-{turn}"),"role":"assistant",
                 "content":[{"type":"output_text","text":"answer","annotations":[]}]}
            ]),
            "source",
        );
        let ConversationItem::Assistant(assistant) = request.items.last().unwrap() else {
            panic!()
        };
        assert_eq!(assistant.model_id.as_deref(), Some("test"));
        persisted.extend(request.items[input_count..].iter().cloned());
        persisted.push(ConversationItem::system_reminder(format!(
            "runtime reminder {turn}"
        )));
        persisted.push(ConversationItem::user(format!("next {turn}")));
        // resume 序列化往返和每轮重新归一化不能使完整前缀失配。
        persisted = serde_json::from_str(&serde_json::to_string(&persisted).unwrap()).unwrap();
        let mut next = prepare(&persisted);
        next.tools.push(ToolSpec {
            name: format!("new_tool_{turn}"),
            description: None,
            parameters: serde_json::json!({"type":"object"}),
        });
        next.x_grok_req_id = Some(format!("request-{turn}"));
        let input = source_history(&next, Some("source"));
        assert_eq!(input[1]["encrypted_content"], format!("checkpoint-{turn}"));
        assert_eq!(input.as_array().unwrap().len(), 5);
    }
}

#[test]
fn responses_checkpoint_changed_system_recovers_after_new_checkpoint() {
    let mut req = ConversationRequest::from_items(vec![
        ConversationItem::system("old memory"),
        ConversationItem::user("task"),
    ]);
    append_checkpoint(
        &mut req,
        serde_json::json!([
            {"type":"compaction","encrypted_content":"old_checkpoint"}
        ]),
        "source",
    );
    req.items[0] = ConversationItem::system("new memory");
    let input = source_history(&req, Some("source"));
    assert!(!input.to_string().contains("old_checkpoint"));
    assert_eq!(input[0]["content"], "new memory");
    append_checkpoint(
        &mut req,
        serde_json::json!([
            {"type":"compaction","encrypted_content":"fresh_checkpoint"}
        ]),
        "source",
    );
    let input = source_history(&req, Some("source"));
    assert_eq!(input[1]["encrypted_content"], "fresh_checkpoint");
    assert_eq!(input.as_array().unwrap().len(), 2);
}

#[test]
fn responses_checkpoint_tool_image_cleanup_invalidates_entire_chain() {
    let mut req = ConversationRequest::from_items(vec![
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: Arc::from("c1"),
            name: "read_file".into(),
            arguments: Arc::from("{}"),
        }]),
        ConversationItem::ToolResult(ToolResultItem {
            tool_call_id: "c1".into(),
            content: Arc::from("image result"),
            images: vec![ContentPart::Image {
                url: Arc::from("data:image/png;base64,old"),
            }],
        }),
    ]);
    for id in ["first_checkpoint", "second_checkpoint"] {
        append_checkpoint(
            &mut req,
            serde_json::json!([
                {"type":"compaction","encrypted_content":id}
            ]),
            "source",
        );
    }
    assert_eq!(
        source_history(&req, Some("source"))[0]["encrypted_content"],
        "second_checkpoint"
    );
    let ConversationItem::ToolResult(result) = &mut req.items[1] else {
        panic!()
    };
    result.images.clear();
    let input = source_history(&req, Some("source"));
    assert!(!input.to_string().contains("checkpoint"));
    assert!(!input.to_string().contains("base64"));
    assert_eq!(input[0]["type"], "function_call");
    assert_eq!(input[1]["type"], "function_call_output");
}

/// 构造已由客户端绑定来源和完整准备历史的响应。
fn signed_context_response(
    history: &ConversationRequest,
    raw: serde_json::Value,
    total_tokens: Option<u32>,
) -> rs::Response {
    let mut response = raw_history_response(raw);
    let metadata = response.metadata.as_mut().unwrap();
    metadata.insert(RESPONSE_SOURCE_METADATA_KEY.into(), "source".into());
    metadata.insert(
        RESPONSE_INPUT_FINGERPRINT_METADATA_KEY.into(),
        responses_input_fingerprint(history, "source"),
    );
    response.usage = total_tokens.map(|tokens| {
        serde_json::from_value(serde_json::json!({
            "input_tokens": tokens, "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 0, "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": tokens
        }))
        .expect("response usage fixture")
    });
    response
}

/// 走完整签发和持久历史追加路径，不直接伪造内部记录。
fn append_context_response(
    history: &mut ConversationRequest,
    raw: serde_json::Value,
    context_tokens: Option<u64>,
    total_tokens: Option<u32>,
    tools: &serde_json::Value,
) -> (ResponsesContextUsage, rs::Response) {
    let mut response = signed_context_response(history, raw, total_tokens);
    let usage = checkpoint_responses_context(
        &mut response,
        history,
        "source",
        tools,
        context_tokens,
        std::num::NonZeroU64::new(1_000).unwrap(),
    )
    .unwrap();
    history
        .items
        .extend(response_to_conversation_items(response.clone()));
    (usage, response)
}

/// 读取末尾响应记录，验证持久化结构而不是进程内状态。
fn last_context_record(history: &ConversationRequest) -> &serde_json::Value {
    let Some(ConversationItem::Assistant(assistant)) = history.items.last() else {
        panic!()
    };
    assistant
        .responses_output
        .as_ref()
        .unwrap()
        .first()
        .unwrap()
        .get(RESPONSE_CONTEXT_CHECKPOINT_FIELD)
        .unwrap()
}

#[test]
fn responses_context_projection_matches_source_wrapper() {
    let mut history = ConversationRequest::from_items(vec![
        ConversationItem::system("sys"),
        ConversationItem::user("old"),
    ]);
    append_checkpoint(
        &mut history,
        serde_json::json!([
            {"type":"compaction", "encrypted_content":"opaque"},
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"answer"}]}
        ]),
        "source",
    );
    for source in [None, Some(""), Some("source"), Some("other")] {
        let projection = project_responses_input(&history, source);
        assert_eq!(
            serde_json::json!(projection.input),
            source_history(&history, source)
        );
        assert_eq!(
            projection.checkpoint_key.is_some(),
            source == Some("source")
        );
    }
    let mut legacy = serde_json::json!({"model":"untouched"});
    patch_responses_input(&mut legacy, &history);
    assert_eq!(legacy["model"], "untouched");
    assert!(!legacy["input"].to_string().contains("opaque"));
}

#[test]
fn responses_context_checkpoint_survives_serde_and_estimates_appended_tail() {
    let tools = serde_json::json!([]);
    let mut history =
        ConversationRequest::from_items(vec![ConversationItem::user("old".repeat(2_000))]);
    let (usage, _) = append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"compaction", "encrypted_content":"opaque"},
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"answer"}]}
        ]),
        Some(75),
        Some(50_000),
        &tools,
    );
    assert_eq!(
        usage,
        ResponsesContextUsage {
            tokens: 75,
            estimated: false
        }
    );
    assert!(
        last_context_record(&history)
            .get("replacement_input")
            .is_some()
    );
    history.items = serde_json::from_str(&serde_json::to_string(&history.items).unwrap()).unwrap();
    let projection = project_responses_input(&history, Some("source"));
    assert_eq!(responses_projection_context(&projection, &tools), usage);
    assert!(
        !serde_json::to_string(&projection.input)
            .unwrap()
            .contains("__grok_context")
    );
    history
        .items
        .push(ConversationItem::user("a new user message"));
    let extended = project_responses_input(&history, Some("source"));
    assert_eq!(extended.checkpoint_key, projection.checkpoint_key);
    let extended_usage = responses_projection_context(&extended, &tools);
    assert!(extended_usage.tokens > 75);
    assert!(extended_usage.estimated);
    assert!(history.items.len() > projection.input.len());
}

#[test]
fn responses_context_normal_usage_calibrates_without_replacement_input() {
    let tools = serde_json::json!([]);
    let mut history = ConversationRequest::from_items(vec![ConversationItem::user("old")]);
    append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"compaction", "encrypted_content":"opaque"}
        ]),
        None,
        Some(50_000),
        &tools,
    );
    history.items.push(ConversationItem::user("next"));
    let (usage, _) = append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"normal answer"}]}
        ]),
        None,
        Some(123),
        &tools,
    );
    assert_eq!(
        usage,
        ResponsesContextUsage {
            tokens: 123,
            estimated: false
        }
    );
    let record = last_context_record(&history);
    assert!(record.get("replacement_input").is_none());
    assert!(record["checkpoint_key"].is_string());
    history.items = serde_json::from_value(serde_json::to_value(&history.items).unwrap()).unwrap();
    assert_eq!(
        responses_projection_context(&project_responses_input(&history, Some("source")), &tools),
        usage
    );
    let Some(ConversationItem::Assistant(assistant)) = history.items.last() else {
        panic!()
    };
    assert_eq!(assistant_context_usage(assistant), Some(usage));
}

#[test]
fn responses_context_details_override_normal_usage_and_missing_usage_is_estimated() {
    let tools = serde_json::json!([]);
    let mut history = ConversationRequest::from_items(vec![ConversationItem::user("task")]);
    let (usage, _) = append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"answer"}]}
        ]),
        Some(47),
        Some(500),
        &tools,
    );
    assert_eq!(
        usage,
        ResponsesContextUsage {
            tokens: 47,
            estimated: false
        }
    );
    let (estimated, _) = append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"more"}]}
        ]),
        None,
        None,
        &tools,
    );
    assert!(estimated.estimated);
    assert!(estimated.tokens > 47);
}

#[test]
fn responses_context_source_history_and_sibling_changes_invalidate_records() {
    let tools = serde_json::json!([]);
    let mut history =
        ConversationRequest::from_items(vec![ConversationItem::user("old".repeat(1_000))]);
    append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"reasoning", "id":"r1", "summary":[], "encrypted_content":"reason"},
            {"type":"compaction", "encrypted_content":"opaque"}
        ]),
        Some(1),
        None,
        &tools,
    );
    for source in [None, Some("other")] {
        let projection = project_responses_input(&history, source);
        assert!(projection.checkpoint_key.is_none());
        let usage = responses_projection_context(&projection, &tools);
        assert!(usage.estimated && usage.tokens > 1);
    }
    for change in ["prefix", "sibling", "same-length-prefix", "assistant"] {
        let mut edited = history.clone();
        match change {
            "prefix" => {
                edited.items.remove(0);
            }
            "same-length-prefix" => {
                edited.items[0] = ConversationItem::user("new".repeat(1_000));
            }
            "sibling" => {
                edited
                    .items
                    .retain(|item| !matches!(item, ConversationItem::Reasoning(_)));
            }
            "assistant" => {
                let Some(ConversationItem::Assistant(assistant)) = edited.items.last_mut() else {
                    panic!()
                };
                assistant.content = Arc::from("rewritten");
            }
            _ => unreachable!(),
        }
        let projection = project_responses_input(&edited, Some("source"));
        assert!(projection.checkpoint_key.is_none(), "{change}");
        assert!(
            responses_projection_context(&projection, &tools).estimated,
            "{change}"
        );
    }
}

#[test]
fn responses_context_unsafe_new_boundary_cannot_commit_low_count() {
    let tools = serde_json::json!([]);
    let mut history =
        ConversationRequest::from_items(vec![ConversationItem::user("old".repeat(1_000))]);
    let (usage, _) = append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"function_call", "call_id":"c1", "name":"read_file", "arguments":"{}"},
            {"type":"compaction", "encrypted_content":"unsafe"}
        ]),
        Some(1),
        Some(1),
        &tools,
    );
    assert!(usage.estimated && usage.tokens > 1);
    assert!(
        last_context_record(&history)
            .get("replacement_input")
            .is_none()
    );
    history
        .items
        .push(ConversationItem::tool_result("c1", "result"));
    let projection = project_responses_input(&history, Some("source"));
    assert!(projection.checkpoint_key.is_none());
    assert!(
        !serde_json::to_string(&projection.input)
            .unwrap()
            .contains("unsafe")
    );
}

#[test]
fn responses_context_pending_tool_tail_does_not_calibrate_full_history_with_compact_usage() {
    let tools = serde_json::json!([]);
    for context_tokens in [None, Some(17)] {
        let mut history =
            ConversationRequest::from_items(vec![ConversationItem::user("old".repeat(1_000))]);
        append_context_response(
            &mut history,
            serde_json::json!([
                {"type":"compaction", "encrypted_content":"safe"}
            ]),
            Some(100),
            None,
            &tools,
        );
        let (usage, _) = append_context_response(
            &mut history,
            serde_json::json!([
                {"type":"function_call", "call_id":"c1", "name":"read_file", "arguments":"{}"}
            ]),
            context_tokens,
            Some(1),
            &tools,
        );
        let reported = ResponsesContextUsage {
            tokens: context_tokens.unwrap_or(1),
            estimated: false,
        };
        assert!(usage.estimated && usage.tokens > 1_000);
        assert_eq!(
            last_context_record(&history)["usage"]["tokens"],
            usage.tokens
        );
        assert_eq!(
            last_context_record(&history)["reported_usage"],
            serde_json::json!(reported)
        );
        history.items =
            serde_json::from_str(&serde_json::to_string(&history.items).unwrap()).unwrap();
        let Some(ConversationItem::Assistant(assistant)) = history.items.last() else {
            panic!()
        };
        assert_eq!(assistant_context_usage(assistant), Some(reported));
        let pending = project_responses_input(&history, Some("source"));
        assert!(pending.checkpoint_key.is_none());
        assert_eq!(responses_projection_context(&pending, &tools), usage);

        // 缺少新字段的旧记录仍返回原安全预算。
        let mut legacy = history.clone();
        let Some(ConversationItem::Assistant(assistant)) = legacy.items.last_mut() else {
            panic!()
        };
        assistant
            .responses_output
            .as_mut()
            .unwrap()
            .first_mut()
            .unwrap()
            .get_mut(RESPONSE_CONTEXT_CHECKPOINT_FIELD)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("reported_usage");
        assert_eq!(assistant_context_usage(assistant), Some(usage));
        assert_eq!(
            responses_projection_context(&project_responses_input(&legacy, Some("source")), &tools),
            usage
        );

        history
            .items
            .push(ConversationItem::tool_result("c1", "result"));
        history.items =
            serde_json::from_str(&serde_json::to_string(&history.items).unwrap()).unwrap();
        let projection = project_responses_input(&history, Some("source"));
        assert!(projection.checkpoint_key.is_some());
        assert_eq!(
            responses_projection_context(&projection, &tools),
            ResponsesContextUsage {
                tokens: 106,
                estimated: true
            },
        );
    }
}

#[test]
fn responses_context_legacy_compaction_metadata_remains_source_bound() {
    let tools = serde_json::json!([]);
    let mut history = ConversationRequest::from_items(vec![ConversationItem::user("old")]);
    append_checkpoint(
        &mut history,
        serde_json::json!([
            {"type":"compaction", "encrypted_content":"opaque", "__grok_context_tokens":41, "__grok_context_estimated":false}
        ]),
        "source",
    );
    let projection = project_responses_input(&history, Some("source"));
    assert_eq!(
        responses_projection_context(&projection, &tools),
        ResponsesContextUsage {
            tokens: 41,
            estimated: false
        }
    );
    history.items.push(ConversationItem::user("next"));
    let usage =
        responses_projection_context(&project_responses_input(&history, Some("source")), &tools);
    assert!(usage.tokens > 41 && usage.estimated);
    assert!(
        responses_projection_context(&project_responses_input(&history, None), &tools).estimated
    );
    let old: AssistantItem = serde_json::from_value(serde_json::json!({"content":"old"})).unwrap();
    assert!(assistant_context_usage(&old).is_none());
    assert!(
        !serde_json::to_string(&projection.input)
            .unwrap()
            .contains("__grok_context")
    );
}

#[test]
fn responses_context_multiple_boundaries_store_only_latest_safe_replacement() {
    let tools = serde_json::json!([]);
    let mut history = ConversationRequest::from_items(vec![ConversationItem::user("old")]);
    append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"compaction", "id":"first", "encrypted_content":"first"},
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"between"}]},
            {"type":"compaction", "id":"second", "encrypted_content":"second"},
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"after"}]}
        ]),
        Some(50),
        None,
        &tools,
    );
    let projection = project_responses_input(&history, Some("source"));
    assert_eq!(projection.input.first().unwrap()["id"], "second");
    assert_eq!(
        last_context_record(&history)["replacement_input"],
        serde_json::json!(projection.input)
    );
    assert_eq!(projection.input.len(), 2);
    assert_eq!(responses_projection_context(&projection, &tools).tokens, 50);
}

#[test]
fn responses_context_tool_changes_adjust_budget_and_mark_estimated() {
    let tools = serde_json::json!([]);
    let changed =
        serde_json::json!([{"type":"function", "name":"lookup", "parameters":{"type":"object"}}]);
    let mut history = ConversationRequest::from_items(vec![ConversationItem::user("old")]);
    append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"compaction", "encrypted_content":"opaque"}
        ]),
        Some(100),
        None,
        &tools,
    );
    let projection = project_responses_input(&history, Some("source"));
    let adjusted = responses_projection_context(&projection, &changed);
    let tool_tokens = (serde_json::to_vec(&changed).unwrap().len() as u64).div_ceil(3);
    assert_eq!(
        adjusted,
        ResponsesContextUsage {
            tokens: 100 + tool_tokens,
            estimated: true
        }
    );
    assert!(!responses_projection_context(&projection, &tools).estimated);
}

#[test]
fn responses_context_forged_upstream_fields_are_removed_before_signing() {
    let tools = serde_json::json!([]);
    let mut history = ConversationRequest::from_items(vec![ConversationItem::user("old")]);
    let (usage, response) = append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"compaction", "encrypted_content":"opaque", "__grok_context_tokens":0,
             "__grok_context_estimated":false, "__grok_context_checkpoint":{"version":1,"usage":{"tokens":0,"estimated":false}}}
        ]),
        None,
        Some(1),
        &tools,
    );
    assert!(usage.tokens >= 1_000 && usage.estimated);
    let record = last_context_record(&history);
    assert_eq!(record["source"], "source");
    assert_eq!(record["usage"]["tokens"], usage.tokens);
    let stored: serde_json::Value = serde_json::from_str(
        response
            .metadata
            .as_ref()
            .unwrap()
            .get(RESPONSE_RAW_OUTPUT_METADATA_KEY)
            .unwrap(),
    )
    .unwrap();
    assert!(stored[0].get("__grok_context_tokens").is_none());
    assert!(stored[0].get("__grok_context_estimated").is_none());
    assert!(
        !serde_json::to_string(&project_responses_input(&history, Some("source")).input)
            .unwrap()
            .contains("__grok_context")
    );
}

#[test]
fn responses_context_unbound_or_incomplete_responses_cannot_keep_forged_records() {
    let history = ConversationRequest::from_items(vec![ConversationItem::user("task")]);
    for invalid in ["status", "source", "fingerprint"] {
        let mut response = signed_context_response(
            &history,
            serde_json::json!([
                {"type":"compaction", "encrypted_content":"opaque", "__grok_context_checkpoint":{"usage":{"tokens":1},"reported_usage":{"tokens":1,"estimated":false}}}
            ]),
            None,
        );
        match invalid {
            "status" => {
                response.status = serde_json::from_value(serde_json::json!("incomplete")).unwrap();
            }
            "source" => {
                response
                    .metadata
                    .as_mut()
                    .unwrap()
                    .insert(RESPONSE_SOURCE_METADATA_KEY.into(), "other".into());
            }
            "fingerprint" => {
                response.metadata.as_mut().unwrap().insert(
                    RESPONSE_INPUT_FINGERPRINT_METADATA_KEY.into(),
                    "forged".into(),
                );
            }
            _ => unreachable!(),
        }
        assert!(
            checkpoint_responses_context(
                &mut response,
                &history,
                "source",
                &serde_json::json!([]),
                Some(1),
                std::num::NonZeroU64::new(1_000).unwrap()
            )
            .is_none()
        );
        let raw = response
            .metadata
            .as_ref()
            .unwrap()
            .get(RESPONSE_RAW_OUTPUT_METADATA_KEY)
            .unwrap();
        assert!(!raw.contains(RESPONSE_CONTEXT_CHECKPOINT_FIELD));
        let items = response_to_conversation_items(response);
        let Some(ConversationItem::Assistant(assistant)) = items.last() else {
            panic!()
        };
        assert!(assistant_context_usage(assistant).is_none());
    }
}

#[test]
fn responses_context_invalid_record_prefix_version_and_key_are_not_reused() {
    let tools = serde_json::json!([]);
    let mut history = ConversationRequest::from_items(vec![ConversationItem::user("old")]);
    append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"compaction", "encrypted_content":"opaque"}
        ]),
        Some(1),
        None,
        &tools,
    );
    for (field, value) in [
        ("version", serde_json::json!(2)),
        ("input_len", serde_json::json!(999_999)),
        ("input_hash", serde_json::json!("forged")),
        ("checkpoint_key", serde_json::json!("other")),
        ("replacement_input", serde_json::json!([])),
    ] {
        let mut edited = history.clone();
        let Some(ConversationItem::Assistant(assistant)) = edited.items.last_mut() else {
            panic!()
        };
        let record = assistant
            .responses_output
            .as_mut()
            .unwrap()
            .first_mut()
            .unwrap()
            .get_mut(RESPONSE_CONTEXT_CHECKPOINT_FIELD)
            .unwrap()
            .as_object_mut()
            .unwrap();
        record.insert(field.into(), value);
        let usage =
            responses_projection_context(&project_responses_input(&edited, Some("source")), &tools);
        assert!(usage.estimated, "{field}");
    }
}

#[test]
fn responses_context_image_estimate_is_bounded_and_ciphertext_is_never_exact() {
    let history = ConversationRequest::from_items(vec![ConversationItem::user_with_parts(vec![
        ContentPart::Image {
            url: Arc::from(format!("data:image/png;base64,{}", "a".repeat(100_000))),
        },
    ])]);
    let projection = project_responses_input(&history, None);
    let usage = responses_projection_context(&projection, &serde_json::json!([]));
    assert!(usage.estimated && usage.tokens >= 1_024 && usage.tokens < 2_000);
    let mut history = ConversationRequest::from_items(vec![]);
    append_checkpoint(
        &mut history,
        serde_json::json!([
            {"type":"compaction", "encrypted_content":"a".repeat(1_000)}
        ]),
        "source",
    );
    assert!(
        responses_projection_context(
            &project_responses_input(&history, Some("source")),
            &serde_json::json!([])
        )
        .estimated
    );
}

#[test]
fn responses_context_empty_output_and_response_reader_keep_calibration() {
    let tools = serde_json::json!([]);
    let mut history = ConversationRequest::from_items(vec![ConversationItem::user("task")]);
    let (usage, response) =
        append_context_response(&mut history, serde_json::json!([]), None, Some(64), &tools);
    let mut converted = ConversationResponse {
        items: response_to_conversation_items(response.clone()),
        stop_reason: Some(StopReason::Stop),
        usage: None,
        cost_usd_ticks: None,
        message_chunks_emitted: 0,
        doom_loop_signals: Vec::new(),
        stop_message: None,
        message_id: None,
        raw_stop_reason: None,
        stop_sequence: None,
    };
    assert_eq!(response_context_usage(&converted), Some(usage));
    assert_eq!(
        responses_projection_context(&project_responses_input(&history, Some("source")), &tools),
        usage
    );
    let mut incomplete = response;
    incomplete.status = serde_json::from_value(serde_json::json!("incomplete")).unwrap();
    converted.items = response_to_conversation_items(incomplete);
    assert!(response_context_usage(&converted).is_none());
}

#[test]
fn responses_context_skipped_last_boundary_cannot_supply_exact_details() {
    let tools = serde_json::json!([]);
    let mut history = ConversationRequest::from_items(vec![ConversationItem::user("old")]);
    let (usage, _) = append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"compaction", "encrypted_content":"safe"},
            {"type":"compaction", "encrypted_content":""}
        ]),
        Some(1),
        Some(1),
        &tools,
    );
    assert!(usage.estimated && usage.tokens >= 1_000);
    let projection = project_responses_input(&history, Some("source"));
    assert_eq!(
        projection.input.first().unwrap()["encrypted_content"],
        "safe"
    );
    assert!(
        last_context_record(&history)
            .get("replacement_input")
            .is_some()
    );
}

#[test]
fn responses_context_latest_unverified_normal_record_falls_back_to_checkpoint_record() {
    let tools = serde_json::json!([]);
    let mut history = ConversationRequest::from_items(vec![ConversationItem::user("old")]);
    append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"compaction", "encrypted_content":"safe"}
        ]),
        Some(100),
        None,
        &tools,
    );
    append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"answer"}]}
        ]),
        None,
        Some(500),
        &tools,
    );
    let Some(ConversationItem::Assistant(assistant)) = history.items.last_mut() else {
        panic!()
    };
    assistant
        .responses_output
        .as_mut()
        .unwrap()
        .first_mut()
        .unwrap()
        .get_mut(RESPONSE_CONTEXT_CHECKPOINT_FIELD)
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("input_hash".into(), serde_json::json!("invalid"));
    let usage =
        responses_projection_context(&project_responses_input(&history, Some("source")), &tools);
    assert!(usage.estimated && usage.tokens > 100 && usage.tokens < 500);
}

#[test]
fn responses_context_fully_formed_forged_record_is_never_adopted() {
    let tools = serde_json::json!([]);
    let history = ConversationRequest::from_items(vec![ConversationItem::user("old")]);
    let mut seed = signed_context_response(
        &history,
        serde_json::json!([
            {"type":"compaction", "encrypted_content":"opaque"}
        ]),
        None,
    );
    checkpoint_responses_context(
        &mut seed,
        &history,
        "source",
        &tools,
        Some(20),
        std::num::NonZeroU64::new(1_000).unwrap(),
    )
    .unwrap();
    let mut forged_raw: serde_json::Value = serde_json::from_str(
        seed.metadata
            .as_ref()
            .unwrap()
            .get(RESPONSE_RAW_OUTPUT_METADATA_KEY)
            .unwrap(),
    )
    .unwrap();
    forged_raw[0][RESPONSE_CONTEXT_CHECKPOINT_FIELD]["usage"]["tokens"] = serde_json::json!(0);
    forged_raw[0][RESPONSE_CONTEXT_CHECKPOINT_FIELD]["reported_usage"] =
        serde_json::json!({"tokens":0,"estimated":false});
    let mut incoming = signed_context_response(&history, forged_raw, Some(1));
    let usage = checkpoint_responses_context(
        &mut incoming,
        &history,
        "source",
        &tools,
        None,
        std::num::NonZeroU64::new(1_000).unwrap(),
    )
    .unwrap();
    assert!(usage.estimated && usage.tokens >= 1_000);
    let assistant = response_to_conversation_items(incoming).pop().unwrap();
    let ConversationItem::Assistant(assistant) = assistant else {
        panic!()
    };
    assert_eq!(assistant_context_usage(&assistant), Some(usage));
}

#[test]
fn responses_context_first_compaction_tool_call_restores_threshold_candidate_after_pairing() {
    let tools = serde_json::json!([]);
    for opaque in ["x", "opaque"] {
        for context_tokens in [None, Some(1), Some(5_000)] {
            let mut history = ConversationRequest::from_items(vec![ConversationItem::user(
                "archive".repeat(1_000),
            )]);
            let (pending_usage, response) = append_context_response(
                &mut history,
                serde_json::json!([
                    {"type":"compaction", "encrypted_content":opaque,
                     "__grok_context_tokens":1, "__grok_context_estimated":false,
                     "__grok_context_checkpoint":{"version":1,"usage":{"tokens":1,"estimated":false}}},
                    {"type":"function_call", "call_id":"c1", "name":"read_file", "arguments":"{}"}
                ]),
                context_tokens,
                Some(1),
                &tools,
            );
            let pending = project_responses_input(&history, Some("source"));
            assert!(pending.checkpoint_key.is_none());
            assert!(pending_usage.estimated && pending_usage.tokens > 1_000);
            let record = last_context_record(&history);
            assert!(record["checkpoint_key"].is_null());
            assert!(record.get("replacement_input").is_none());
            assert_eq!(record["usage"]["tokens"], pending_usage.tokens);
            let expected_display = context_tokens
                .map(|tokens| ResponsesContextUsage {
                    tokens,
                    estimated: false,
                })
                .unwrap_or(pending_usage);
            if context_tokens.is_none() {
                assert!(record.get("reported_usage").is_none());
            } else {
                assert_eq!(
                    record["reported_usage"],
                    serde_json::json!(expected_display)
                );
            }
            let Some(ConversationItem::Assistant(assistant)) = history.items.last() else {
                panic!()
            };
            assert_eq!(assistant_context_usage(assistant), Some(expected_display));
            assert_eq!(
                responses_projection_context(&pending, &tools),
                pending_usage
            );
            let raw: serde_json::Value = serde_json::from_str(
                response
                    .metadata
                    .as_ref()
                    .unwrap()
                    .get(RESPONSE_RAW_OUTPUT_METADATA_KEY)
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(raw[0]["__grok_context_tokens"], 1_000);
            assert_eq!(raw[0]["__grok_context_estimated"], true);
            assert_eq!(
                raw[0][RESPONSE_CONTEXT_CHECKPOINT_FIELD]["source"],
                "source"
            );

            // 先恢复待执行工具的历史，再追加真实结果并再次恢复。
            history.items =
                serde_json::from_str(&serde_json::to_string(&history.items).unwrap()).unwrap();
            assert!(
                project_responses_input(&history, Some("source"))
                    .checkpoint_key
                    .is_none()
            );
            history
                .items
                .push(ConversationItem::tool_result("c1", "result"));
            history.items =
                serde_json::from_str(&serde_json::to_string(&history.items).unwrap()).unwrap();
            let paired = project_responses_input(&history, Some("source"));
            assert!(paired.checkpoint_key.is_some());
            assert_eq!(paired.input.len(), 3);
            assert_eq!(paired.input.first().unwrap()["encrypted_content"], opaque);
            // threshold 1000 + read_file 3 + {} 1 + result 2；不累计 archive 或 context_details 尾部。
            assert_eq!(
                responses_projection_context(&paired, &tools),
                ResponsesContextUsage {
                    tokens: 1_006,
                    estimated: true
                },
            );
            assert!(
                !serde_json::to_string(&paired.input)
                    .unwrap()
                    .contains("__grok_context")
            );
            assert!(
                project_responses_input(&history, None)
                    .checkpoint_key
                    .is_none()
            );
            assert!(
                project_responses_input(&history, Some("other"))
                    .checkpoint_key
                    .is_none()
            );
            history.items[0] = ConversationItem::user("modified archive");
            assert!(
                project_responses_input(&history, Some("source"))
                    .checkpoint_key
                    .is_none()
            );
        }
    }
}

#[test]
fn responses_context_pending_candidate_waits_for_all_real_tool_results() {
    let tools = serde_json::json!([]);
    let mut history =
        ConversationRequest::from_items(vec![ConversationItem::user("archive".repeat(1_000))]);
    append_context_response(
        &mut history,
        serde_json::json!([
            {"type":"compaction", "encrypted_content":"x", "__grok_context_tokens":999_999},
            {"type":"function_call", "call_id":"c1", "name":"read_file", "arguments":"{}"},
            {"type":"function_call", "call_id":"c2", "name":"read_file", "arguments":"{}"}
        ]),
        Some(1),
        Some(1),
        &tools,
    );
    history
        .items
        .push(ConversationItem::tool_result("c1", "result"));
    let partial = project_responses_input(&history, Some("source"));
    assert!(partial.checkpoint_key.is_none());
    assert!(responses_projection_context(&partial, &tools).tokens > 1_000);
    history
        .items
        .push(ConversationItem::tool_result("c2", "result"));
    history.items = serde_json::from_str(&serde_json::to_string(&history.items).unwrap()).unwrap();
    let complete = project_responses_input(&history, Some("source"));
    assert!(complete.checkpoint_key.is_some());
    assert_eq!(
        responses_projection_context(&complete, &tools),
        ResponsesContextUsage {
            tokens: 1_012,
            estimated: true
        },
    );
}
