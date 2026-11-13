//! A small Amazon States Language walker for `statemachines/direct.asl.json`: Task (Lambda invoke, the asynchronous
//! `InvocationType: Event` call and `waitForTaskToken`), Choice, Pass, Succeed and Fail, `Parameters`, `ResultSelector`,
//! `ResultPath` (a `null` one in a Task or in a Catch leaves the state as it was), `Retry` with attempt
//! counting (no sleeping), `Catch`, `$$.Task.Token` and the one intrinsic function the definition uses,
//! `States.JsonMerge`. It runs the real definition against in-process fakes of the four Lambdas, so a wrong path, a
//! missing key or a mistyped error name fails a test instead of a deployed run.
//!
//! Not a general interpreter: a state type or field it does not know is a panic, so the definition cannot grow past
//! what the tests cover without the walker growing with it.
#![allow(clippy::unwrap_used)]

use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The Lambda function names the substitutions stand for.
pub const FUNCTIONS: [(&str, &str); 4] = [
    ("${RunSessionArn}", "run-session"),
    ("${RepoGatewayArn}", "repo-gateway"),
    ("${BundleValidatorArn}", "bundle-validator"),
    ("${PackagerArn}", "packager"),
];

/// How a `waitForTaskToken` state ends after its Lambda returned.
#[derive(Clone, Debug)]
pub enum Wait {
    Success(Value),
    HeartbeatTimeout,
    Timeout,
    Fail(String, String),
}

pub trait Lambdas {
    /// The Lambda's payload, or `(errorType, errorMessage)`.
    fn invoke(&mut self, function: &str, payload: Value) -> Result<Value, (String, String)>;
    /// An asynchronous call (`InvocationType: Event`): Lambda queues the event and answers 202 at once, so the only failure the
    /// machine sees is the call itself failing, as `(errorType, errorMessage)`. Whatever the function does with the event is not part
    /// of the walk. A fake that has no asynchronous call refuses it.
    fn event(&mut self, function: &str, payload: Value) -> Result<(), (String, String)> {
        let _ = payload;
        panic!("event not supported by this fake: {function}")
    }
    /// What happens to the task after `invoke` of a `waitForTaskToken` state returned (`token` is this attempt's).
    fn wait(&mut self, function: &str, token: &str) -> Wait;
}

#[derive(Debug)]
pub struct Walked {
    pub output: Value,
    /// Every state entered, in order (a retried state appears once per entry, not per attempt).
    pub trace: Vec<String>,
    /// The same states with the data each one was entered with.
    pub entered: Vec<(String, Value)>,
    /// `(state, retries)` for the states that retried.
    pub retries: Vec<(String, u32)>,
    /// `(state, error name)` of every Catch that fired.
    pub caught: Vec<(String, String)>,
    /// `(Error, Cause)` of the Fail state the execution ended in; `None` for an execution that ended in a Succeed state or at
    /// a state with `End`.
    pub failed: Option<(String, String)>,
}

pub fn definition(path: &str) -> Value {
    let mut text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    for (placeholder, name) in FUNCTIONS {
        text = text.replace(placeholder, name);
    }
    serde_json::from_str(&text).unwrap()
}

pub fn direct_definition() -> Value {
    definition(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../statemachines/direct.asl.json"
    ))
}

/// `statemachines/direct-run-ended.asl.json`: the machine the run-ended rule starts.
pub fn reaper_definition() -> Value {
    definition(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../statemachines/direct-run-ended.asl.json"
    ))
}

fn get<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let rest = path.strip_prefix('$')?;
    let mut cur = v;
    for part in rest.split('.').filter(|p| !p.is_empty()) {
        cur = cur.get(part)?;
    }
    Some(cur)
}

fn set(root: &mut Value, path: &str, value: Value) {
    if path == "$" {
        *root = value;
        return;
    }
    let parts: Vec<&str> = path
        .strip_prefix("$.")
        .unwrap_or_else(|| panic!("ResultPath {path}"))
        .split('.')
        .collect();
    let mut cur = root;
    for (i, p) in parts.iter().enumerate() {
        if !cur.is_object() {
            *cur = json!({});
        }
        let map = cur.as_object_mut().unwrap();
        if i + 1 == parts.len() {
            map.insert((*p).to_string(), value);
            return;
        }
        cur = map.entry((*p).to_string()).or_insert_with(|| json!({}));
    }
}

struct Ctx<'a> {
    input: &'a Value,
    token: Option<&'a str>,
}

fn resolve(path: &str, cx: &Ctx<'_>) -> Value {
    if path == "$$.Task.Token" {
        return json!(
            cx.token
                .expect("$$.Task.Token outside a waitForTaskToken state")
        );
    }
    get(cx.input, path)
        .unwrap_or_else(|| panic!("the path {path} does not exist in {}", cx.input))
        .clone()
}

fn intrinsic(expr: &str, cx: &Ctx<'_>) -> Value {
    let inner = expr
        .strip_prefix("States.JsonMerge(")
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or_else(|| panic!("unsupported intrinsic function {expr}"));
    let args: Vec<&str> = inner.split(',').map(str::trim).collect();
    assert_eq!(args.len(), 3, "{expr}");
    assert_eq!(
        args[2], "false",
        "{expr}: only a shallow merge is supported"
    );
    let (a, b) = (resolve(args[0], cx), resolve(args[1], cx));
    let mut out: Map<String, Value> = a.as_object().expect("JsonMerge needs objects").clone();
    for (k, v) in b.as_object().expect("JsonMerge needs objects") {
        out.insert(k.clone(), v.clone());
    }
    Value::Object(out)
}

fn evaluate(template: &Value, cx: &Ctx<'_>) -> Value {
    match template {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, v) in m {
                match k.strip_suffix(".$") {
                    Some(name) => {
                        let s = v.as_str().unwrap_or_else(|| panic!("{k} must be a string"));
                        let val = if s.starts_with("States.") {
                            intrinsic(s, cx)
                        } else {
                            resolve(s, cx)
                        };
                        out.insert(name.to_string(), val);
                    }
                    None => {
                        out.insert(k.clone(), evaluate(v, cx));
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| evaluate(x, cx)).collect()),
        other => other.clone(),
    }
}

fn choice(rule: &Value, state: &Value) -> bool {
    let var = rule["Variable"].as_str().unwrap();
    let found = get(state, var);
    if let Some(want) = rule.get("IsPresent").and_then(Value::as_bool) {
        return found.is_some() == want;
    }
    if let Some(want) = rule.get("BooleanEquals").and_then(Value::as_bool) {
        return found.and_then(Value::as_bool) == Some(want);
    }
    panic!("unsupported Choice rule {rule}");
}

fn error_object(name: &str, message: &str) -> Value {
    json!({"Error": name, "Cause": json!({"errorMessage": message, "errorType": name}).to_string()})
}

fn matches(errors: &Value, name: &str) -> bool {
    errors.as_array().unwrap().iter().any(|e| {
        e == name || e == "States.ALL" || (e == "States.TaskFailed" && !name.starts_with("States."))
    })
}

pub fn walk(def: &Value, input: Value, lambdas: &mut dyn Lambdas) -> Walked {
    let states = def["States"].as_object().unwrap();
    let mut name = def["StartAt"].as_str().unwrap().to_string();
    let mut state = input;
    let mut trace = Vec::new();
    let mut entered = Vec::new();
    let mut retries: BTreeMap<String, u32> = BTreeMap::new();
    let mut caught = Vec::new();
    let mut failed = None;
    let mut token_n = 0u32;
    for _ in 0..4_000 {
        let st = &states[&name];
        trace.push(name.clone());
        entered.push((name.clone(), state.clone()));
        match st["Type"].as_str().unwrap() {
            "Choice" => {
                let next = st["Choices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|r| choice(r, &state))
                    .map(|r| r["Next"].as_str().unwrap())
                    .unwrap_or_else(|| {
                        st["Default"]
                            .as_str()
                            .expect("Choice with no match and no Default")
                    });
                name = next.to_string();
            }
            "Pass" => {
                let cx = Ctx {
                    input: &state,
                    token: None,
                };
                let result = match (st.get("Parameters"), st.get("Result")) {
                    (Some(p), _) => evaluate(p, &cx),
                    (None, Some(r)) => r.clone(),
                    (None, None) => state.clone(),
                };
                match st.get("ResultPath").and_then(Value::as_str) {
                    Some(path) => set(&mut state, path, result),
                    None => state = result,
                }
                match st.get("Next").and_then(Value::as_str) {
                    Some(n) => name = n.to_string(),
                    None => break,
                }
            }
            "Succeed" => break,
            "Fail" => {
                // `Error` and `Cause` are text, or `ErrorPath` and `CausePath` name where the state holds them.
                let text = |plain: &str, path: &str| match (st.get(plain), st.get(path)) {
                    (Some(v), None) => v.as_str().unwrap().to_string(),
                    (None, Some(p)) => {
                        let found = get(&state, p.as_str().unwrap())
                            .unwrap_or_else(|| panic!("{path} {p} does not exist in {state}"));
                        found.as_str().unwrap().to_string()
                    }
                    (None, None) => String::new(),
                    (Some(_), Some(_)) => panic!("{name}: {plain} and {path} together"),
                };
                failed = Some((text("Error", "ErrorPath"), text("Cause", "CausePath")));
                break;
            }
            "Task" => {
                let wait_for_token = st["Resource"]
                    .as_str()
                    .unwrap()
                    .ends_with("invoke.waitForTaskToken");
                let mut attempts: BTreeMap<usize, u32> = BTreeMap::new();
                let outcome: Result<Value, (String, String)> = loop {
                    token_n += 1;
                    let token = format!("token-{token_n}");
                    let cx = Ctx {
                        input: &state,
                        token: wait_for_token.then_some(token.as_str()),
                    };
                    let params = evaluate(&st["Parameters"], &cx);
                    let function = params["FunctionName"].as_str().unwrap().to_string();
                    let payload = params.get("Payload").cloned().unwrap_or(Value::Null);
                    let asynchronous =
                        params.get("InvocationType").and_then(Value::as_str) == Some("Event");
                    assert!(
                        !(asynchronous && wait_for_token),
                        "{name}: an Event invocation cannot wait for a task token"
                    );
                    let mut result = if asynchronous {
                        lambdas
                            .event(&function, payload)
                            .map(|()| json!({"StatusCode": 202}))
                    } else {
                        lambdas
                            .invoke(&function, payload)
                            .map(|p| json!({"Payload": p, "StatusCode": 200}))
                    };
                    if wait_for_token && result.is_ok() {
                        result = match lambdas.wait(&function, &token) {
                            Wait::Success(v) => Ok(v),
                            Wait::HeartbeatTimeout => {
                                Err(("States.HeartbeatTimeout".into(), "heartbeat".into()))
                            }
                            Wait::Timeout => Err(("States.Timeout".into(), "timeout".into())),
                            Wait::Fail(e, c) => Err((e, c)),
                        };
                    }
                    match result {
                        Ok(v) => break Ok(v),
                        Err((e, msg)) => {
                            let retrier =
                                st.get("Retry").and_then(Value::as_array).and_then(|rs| {
                                    rs.iter()
                                        .enumerate()
                                        .find(|(_, r)| matches(&r["ErrorEquals"], &e))
                                });
                            match retrier {
                                Some((i, r)) => {
                                    let n = attempts.entry(i).or_insert(0);
                                    if *n < r["MaxAttempts"].as_u64().unwrap_or(3) as u32 {
                                        *n += 1;
                                        *retries.entry(name.clone()).or_insert(0) += 1;
                                        continue;
                                    }
                                    break Err((e, msg));
                                }
                                None => break Err((e, msg)),
                            }
                        }
                    }
                };
                match outcome {
                    Ok(raw) => {
                        let cx = Ctx {
                            input: &raw,
                            token: None,
                        };
                        let result = match st.get("ResultSelector") {
                            Some(sel) => evaluate(sel, &cx),
                            None => raw.clone(),
                        };
                        match st.get("ResultPath") {
                            // `null` discards the result: the state's output is its input.
                            Some(Value::Null) => {}
                            Some(Value::String(path)) => set(&mut state, path, result),
                            None => state = result,
                            Some(other) => panic!("{name}: ResultPath {other}"),
                        }
                        match st.get("Next").and_then(Value::as_str) {
                            Some(n) => name = n.to_string(),
                            None => break,
                        }
                    }
                    Err((e, msg)) => {
                        let catcher = st
                            .get("Catch")
                            .and_then(Value::as_array)
                            .and_then(|cs| cs.iter().find(|c| matches(&c["ErrorEquals"], &e)))
                            .unwrap_or_else(|| {
                                panic!(
                                    "state {name} failed with {e} ({msg}) and has no Catch for it"
                                )
                            });
                        caught.push((name.clone(), e.clone()));
                        match catcher.get("ResultPath") {
                            // `null` drops the error: the next state gets the state's input.
                            Some(Value::Null) => {}
                            Some(Value::String(path)) => {
                                set(&mut state, path, error_object(&e, &msg))
                            }
                            None => state = error_object(&e, &msg),
                            Some(other) => panic!("{name}: Catch ResultPath {other}"),
                        }
                        name = catcher["Next"].as_str().unwrap().to_string();
                    }
                }
            }
            other => panic!("unsupported state type {other} in {name}"),
        }
    }
    Walked {
        output: state,
        trace,
        entered,
        retries: retries.into_iter().collect(),
        caught,
        failed,
    }
}
