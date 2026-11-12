//! 按需装载工具（G 轮）：`deferred` 工具的「先报名、后下发」编排。
//!
//! 问题：工具 schema 随**每一轮**请求重发。MCP 接一台 server 就是十几件工具的完整
//! JSON Schema，而单轮真正用得上的通常只有 1-2 件——其余全是纯 token 税，还挤占
//! 上下文窗口（长会话里这部分是固定成本，与任务难度无关）。
//!
//! 做法（DeepTutor `deferred` + `load_tools` 的同构实现）：
//! 1. `ToolSpec.deferred = Some(true)` 的工具**不进**下发的工具清单；
//! 2. 系统提示词里只留一行名字摘要（`deferred_line`）——模型知道「有这些工具」；
//! 3. 模型调保留工具 `load_tools({"names":[...]})` 报名，自下一轮起进入可见集。
//!
//! 判据边界：`deferred` 只影响**下发**，不影响**调用**。模型凭记忆直接调一个
//! deferred 工具照样成功（`tools.call` 不看该标记）——「猜中即可用」，不惩罚直觉。
//!
//! 为什么不做成 tools guest 的 op：可见性是**编排层**的概念（agent-loop 决定往
//! 请求里放什么），guest 只负责池与执行。放错层会让 tools 侧持有会话态。

use super::*;

/// 名字摘要上限：宁可截断也不让这一行变成新的 token 税。
const MAX_LISTED: usize = 60;

/// `load_tools` 保留工具的声明（随 tools 下发，模型才看得见可调）。
pub(super) fn load_tools_spec() -> ToolSpec {
    ToolSpec {
        name: RESERVED_LOAD_TOOLS.into(),
        obs: None,
        deferred: None,
        description: "Load the full schema of tools whose schemas were withheld to save context. \
Pass the names you need; they become callable from the next round. Call this only when you \
actually intend to use one of the listed on-demand tools."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "names": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Tool names to load (exact names as listed in the on-demand section)"
                }
            },
            "required": ["names"]
        }),
    }
}

/// 系统提示词的按需工具段：只有名字，没有 schema（这正是省下来的那部分）。
/// 名字过多时截断并报总数——一行摘要涨成三十行就又变成税了。
pub(super) fn deferred_line(names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    let shown = if names.len() > MAX_LISTED {
        format!(
            "{} … (+{} more)",
            names[..MAX_LISTED].join(", "),
            names.len() - MAX_LISTED
        )
    } else {
        names.join(", ")
    };
    format!(
        "\n## Tools loaded on demand\n\
         These tools exist but their schemas are NOT in your tool list yet (this saves context): \
{shown}\n\
         Call `load_tools` with {{\"names\":[...]}} to get one; it becomes callable from the next round.\n"
    )
}

/// `load_tools` 的观察结果：只回「装载成功/未命中」的**名字**，不回 schema。
///
/// 为什么不回 schema：schema 下一轮就随工具清单下发，回喂一遍等于同一条信息花两遍
/// token——而 `tool_result` 还另有 8000 字符回喂上限，大 schema 会被腰斩成残缺 JSON。
pub(super) fn load_tools_result(tc: &ToolCall, deferred: &[String]) -> Value {
    let requested: Vec<String> = tc
        .arguments
        .get("names")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if requested.is_empty() {
        return json!({"ok": false, "error": {"code": "K400", "field": "names",
            "message": "load_tools 需非空参数 {\"names\": [str]}（要装载的工具名）"}});
    }
    let mut loaded: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for n in requested {
        if deferred.iter().any(|d| d == &n) {
            if !loaded.contains(&n) {
                loaded.push(n);
            }
        } else {
            unknown.push(n);
        }
    }
    if loaded.is_empty() {
        return json!({"ok": false, "error": {"code": "K400", "field": "names",
            "message": format!("这些名字不在按需工具清单内: {}。可装载的: {}",
                unknown.join(", "), deferred.join(", "))}});
    }
    let mut out = json!({"ok": true, "loaded": loaded, "count": loaded.len(),
        "note": "已装载，自下一轮起可调用（本轮若已调用，tools.call 仍按名字直通）"});
    if !unknown.is_empty() {
        out["ignored"] = json!(unknown);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(names: Value) -> ToolCall {
        ToolCall { id: "c1".into(), name: RESERVED_LOAD_TOOLS.into(), arguments: json!({"names": names}) }
    }

    fn deferred() -> Vec<String> {
        vec!["mcp__fs__read".to_string(), "mcp__fs__write".to_string()]
    }

    #[test]
    fn line_lists_names_without_schemas() {
        let line = deferred_line(&deferred());
        assert!(line.contains("mcp__fs__read"), "{line}");
        assert!(line.contains("load_tools"), "{line}");
        assert!(!line.contains("parameters"), "摘要不得含 schema: {line}");
    }

    #[test]
    fn line_is_empty_when_nothing_deferred() {
        assert_eq!(deferred_line(&[]), "");
    }

    #[test]
    fn line_truncates_but_reports_total() {
        let many: Vec<String> = (0..MAX_LISTED + 5).map(|i| format!("t{i}")).collect();
        let line = deferred_line(&many);
        assert!(line.contains("(+5 more)"), "{line}");
        assert!(!line.contains("t69"), "超出上限的名字不该出现: {line}");
    }

    #[test]
    fn result_reports_names_only() {
        let v = load_tools_result(&call(json!(["mcp__fs__read"])), &deferred());
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["loaded"], json!(["mcp__fs__read"]));
        assert!(v.get("tools").is_none(), "不得回喂 schema: {v}");
    }

    #[test]
    fn result_rejects_unknown_and_empty() {
        let bad = load_tools_result(&call(json!(["nope"])), &deferred());
        assert_eq!(bad["ok"], json!(false));
        assert_eq!(bad["error"]["code"], json!("K400"));
        assert!(bad["error"]["message"].as_str().unwrap().contains("mcp__fs__read"),
            "报错要给出可装载清单: {bad}");

        let empty = load_tools_result(&call(json!([])), &deferred());
        assert_eq!(empty["ok"], json!(false));
    }

    #[test]
    fn result_splits_partial_hits() {
        let v = load_tools_result(&call(json!(["mcp__fs__read", "nope", "mcp__fs__read"])), &deferred());
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["loaded"], json!(["mcp__fs__read"]), "去重");
        assert_eq!(v["ignored"], json!(["nope"]));
    }
}
