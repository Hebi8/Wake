//! File operands from explicit file tools and static code-mode apply_patch calls.
use crate::models::{AgentId, ToolCallView, ToolFile};
use serde_json::Value;

pub(super) fn from_call(
    agent: AgentId,
    call: &ToolCallView,
    idx: usize,
    cwd: &str,
) -> Vec<ToolFile> {
    let Some(input) = call.full_input.as_deref().or(call.input.as_deref()) else {
        return Vec::new();
    };
    let value = serde_json::from_str::<Value>(input).ok();
    let mut paths = Vec::new();
    match call.name.as_str() {
        "exec" if agent == AgentId::Codex && input.contains("apply_patch") => {
            for patch in exec_patches(value.as_ref().and_then(Value::as_str).unwrap_or(input)) {
                patch_paths(&patch, &mut paths);
            }
        }
        "apply_patch" => patch_paths(
            value.as_ref().and_then(Value::as_str).unwrap_or(input),
            &mut paths,
        ),
        "patch" => {
            if let Some(patch) = value
                .as_ref()
                .and_then(|v| v.get("patch"))
                .and_then(Value::as_str)
            {
                patch_paths(patch, &mut paths);
            } else if let Some(path) = field(value.as_ref(), &["path"]) {
                paths.push((path.to_owned(), "change"));
            }
        }
        name => {
            let (fields, op): (&[&str], _) = match name {
                "Edit" | "Write" => (&["file_path"], "change"),
                "Read" => (&["file_path"], "read"),
                "ReadFile" => (&["path"], "read"),
                "NotebookEdit" => (&["notebook_path"], "change"),
                "read" => (&["file_path", "path", "filePath"], "read"),
                "edit" | "write" => (&["file_path", "path", "filePath"], "change"),
                "read_file" => (&["path", "Path"], "read"),
                "write_file" => (&["path"], "change"),
                "view_image" => (&["path"], "read"),
                "view_file" | "client_view_file" => (&["AbsolutePath", "absolute_path"], "read"),
                "replace_file_content"
                | "write_to_file"
                | "client_create_file"
                | "client_edit_file" => (&["TargetFile", "target_file"], "change"),
                _ => return Vec::new(),
            };
            if let Some(path) = field(value.as_ref(), fields) {
                // Antigravity serializes each argument value as JSON inside the args object.
                let path = if agent == AgentId::Antigravity {
                    serde_json::from_str::<String>(path).unwrap_or_else(|_| path.to_owned())
                } else {
                    path.to_owned()
                };
                paths.push((path, op));
            }
        }
    }
    let mut files = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (path, op) in paths {
        let file = ToolFile {
            idx,
            path: resolve(&path, cwd),
            op,
        };
        if seen.insert((file.path.clone(), file.op)) {
            files.push(file);
        }
    }
    files
}

// Code-mode inputs are JavaScript, not shell commands. Read only static strings
// and bindings; opaque expressions stand for patch bodies, never guessed paths.
#[derive(Clone, Copy, PartialEq)]
enum JsToken<'a> {
    Word(&'a str),
    Literal(&'a str),
    Punct(char),
}

// One budget per exec input, shared by parsing, values, copies and loop bodies.
const JS_MAX_DEPTH: usize = 32;
const JS_MAX_STEPS: usize = 16 * 1024 * 1024;
const JS_MAX_ITERATIONS: usize = 4096;
const JS_MAX_STRING: usize = 8 * 1024 * 1024;
const JS_MAX_ITEMS: usize = 4096;
const JS_MAX_BYTES: usize = 64 * 1024 * 1024;
const JS_MAX_INPUT: usize = 16 * 1024 * 1024;
const JS_MAX_TOKENS: usize = 262_144;

#[derive(Default)]
struct JsBudget {
    steps: usize,
    iterations: usize,
    bytes: usize,
    max_depth: usize,
    limited: bool,
    exhausted: bool,
}

impl JsBudget {
    fn step(&mut self, count: usize) -> bool {
        if self.exhausted || count > JS_MAX_STEPS - self.steps {
            self.limited = true;
            self.exhausted = true;
            return false;
        }
        self.steps += count;
        true
    }

    fn allocate(&mut self, bytes: usize) -> bool {
        if self.exhausted || bytes > JS_MAX_BYTES - self.bytes {
            self.limited = true;
            self.exhausted = true;
            return false;
        }
        self.bytes += bytes;
        true
    }

    fn depth(&mut self, depth: usize) -> bool {
        if depth > JS_MAX_DEPTH {
            self.limited = true;
            return false;
        }
        self.max_depth = self.max_depth.max(depth);
        !self.exhausted
    }

    fn items(&mut self, count: usize) -> bool {
        if count > JS_MAX_ITEMS {
            self.limited = true;
            return false;
        }
        true
    }

    fn iteration(&mut self) -> bool {
        if self.iterations == JS_MAX_ITERATIONS {
            self.limited = true;
            self.exhausted = true;
            return false;
        }
        self.iterations += 1;
        self.step(1)
    }

    fn append(&mut self, out: &mut String, text: &str) -> bool {
        if text.len() > JS_MAX_STRING - out.len() {
            self.limited = true;
            return false;
        }
        if !self.step(text.len()) || !self.allocate(text.len()) {
            return false;
        }
        out.push_str(text);
        true
    }

    fn character(&mut self, out: &mut String, c: char) -> bool {
        self.append(out, c.encode_utf8(&mut [0; 4]))
    }

    fn text(&mut self, text: &str) -> Option<String> {
        let mut out = String::new();
        self.append(&mut out, text).then_some(out)
    }
}

enum JsValue {
    Text(String),
    Array(Vec<JsValue>),
    Object(Vec<(String, JsValue)>),
}

fn js_unknown() -> JsValue {
    JsValue::Text("\0".into())
}
type JsBindings<'a> = std::collections::HashMap<&'a str, JsValue>;

fn js_tokens<'a>(code: &'a str, budget: &mut JsBudget) -> Vec<JsToken<'a>> {
    let mut limit = code.len().min(JS_MAX_INPUT);
    while !code.is_char_boundary(limit) {
        limit -= 1;
    }
    budget.limited |= limit != code.len();
    let code = &code[..limit];
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < code.len() {
        if tokens.len() == JS_MAX_TOKENS {
            budget.limited = true;
            break;
        }
        let rest = &code[i..];
        let c = rest.chars().next().unwrap();
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        if rest.starts_with("//") {
            i += rest.find('\n').unwrap_or(rest.len());
            continue;
        }
        if rest.starts_with("/*") {
            i += rest.find("*/").map_or(rest.len(), |n| n + 2);
            continue;
        }
        if !budget.step(1) || !budget.allocate(std::mem::size_of::<JsToken<'_>>()) {
            break;
        }
        if matches!(c, '\'' | '"' | '`') {
            let start = i;
            i += 1;
            while i < code.len() {
                let next = code[i..].chars().next().unwrap();
                i += next.len_utf8();
                if next == '\\' {
                    if let Some(escaped) = code[i..].chars().next() {
                        i += escaped.len_utf8();
                    }
                } else if next == c {
                    break;
                }
            }
            tokens.push(JsToken::Literal(&code[start..i]));
        } else if c.is_ascii_alphanumeric() || matches!(c, '_' | '$') {
            let end = rest
                .find(|x: char| !x.is_ascii_alphanumeric() && x != '_' && x != '$')
                .unwrap_or(rest.len());
            tokens.push(JsToken::Word(&rest[..end]));
            i += end;
        } else {
            tokens.push(JsToken::Punct(c));
            i += c.len_utf8();
        }
    }
    tokens
}

struct JsProgram<'a> {
    tokens: Vec<JsToken<'a>>,
    statements: Vec<usize>,
    groups: Vec<usize>,
}

impl<'a> JsProgram<'a> {
    fn new(code: &'a str, budget: &mut JsBudget) -> Option<Self> {
        let tokens = js_tokens(code, budget);
        let n = tokens.len();
        if !budget.step(n * 2) || !budget.allocate((n + 1) * 2 * std::mem::size_of::<usize>()) {
            return None;
        }
        let mut statements = vec![n; n + 1];
        let mut groups = vec![n; n + 1];
        // Cache each suffix's end once. Assignments in one long statement then
        // take O(1) to find their end, instead of rescanning the same suffix.
        for i in (0..n).rev() {
            match tokens[i] {
                JsToken::Punct(')' | ']' | '}') => {
                    statements[i] = i;
                    groups[i] = i;
                }
                JsToken::Punct(',' | ';') => {
                    statements[i] = i;
                    groups[i] = groups[i + 1];
                }
                JsToken::Punct('(' | '[' | '{') => {
                    let close = groups[i + 1];
                    if close < n {
                        statements[i] = statements[close + 1];
                        groups[i] = groups[close + 1];
                    }
                }
                _ => {
                    statements[i] = statements[i + 1];
                    groups[i] = groups[i + 1];
                }
            }
        }
        Some(Self {
            tokens,
            statements,
            groups,
        })
    }

    fn end(&self, scope: &std::ops::Range<usize>, start: usize, statement: bool) -> usize {
        let ends = if statement {
            &self.statements
        } else {
            &self.groups
        };
        ends[scope.start + start].min(scope.end) - scope.start
    }
}

fn js_copy(value: &JsValue, budget: &mut JsBudget, depth: usize) -> JsValue {
    if !budget.depth(depth) || !budget.step(1) {
        return js_unknown();
    }
    match value {
        JsValue::Text(text) => budget
            .text(text)
            .map(JsValue::Text)
            .unwrap_or_else(js_unknown),
        JsValue::Array(items) => {
            if !budget.items(items.len())
                || !budget.allocate(items.len() * std::mem::size_of::<JsValue>())
            {
                return js_unknown();
            }
            let mut copy = Vec::with_capacity(items.len());
            for item in items {
                let value = js_copy(item, budget, depth + 1);
                if budget.exhausted {
                    return js_unknown();
                }
                copy.push(value);
            }
            JsValue::Array(copy)
        }
        JsValue::Object(items) => {
            if !budget.items(items.len())
                || !budget.allocate(items.len() * std::mem::size_of::<(String, JsValue)>())
            {
                return js_unknown();
            }
            let mut copy = Vec::with_capacity(items.len());
            for (key, value) in items {
                let Some(key) = budget.text(key) else {
                    return js_unknown();
                };
                let value = js_copy(value, budget, depth + 1);
                if budget.exhausted {
                    return js_unknown();
                }
                copy.push((key, value));
            }
            JsValue::Object(copy)
        }
    }
}

fn js_bind<'a>(
    bindings: &mut JsBindings<'a>,
    name: &'a str,
    value: JsValue,
    budget: &mut JsBudget,
) {
    if !budget.step(name.len()) {
        return;
    }
    if !bindings.contains_key(name) && !budget.allocate(std::mem::size_of::<(&str, JsValue)>()) {
        return;
    }
    bindings.insert(name, value);
}

fn js_copy_bindings<'a>(
    bindings: &JsBindings<'a>,
    budget: &mut JsBudget,
    depth: usize,
) -> Option<JsBindings<'a>> {
    let mut copy = JsBindings::new();
    for (&name, value) in bindings {
        let value = js_copy(value, budget, depth);
        js_bind(&mut copy, name, value, budget);
        if budget.exhausted {
            return None;
        }
    }
    Some(copy)
}

fn js_literal(raw: &str, bindings: &JsBindings<'_>, budget: &mut JsBudget) -> Option<String> {
    if !budget.step(raw.len()) {
        return None;
    }
    let quote = raw.chars().next()?;
    let body = raw.strip_prefix(quote)?.strip_suffix(quote)?;
    let mut chars = body.chars().peekable();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        let decoded = if c == '\\' {
            match chars.next()? {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'v' => '\u{b}',
                '0' => '\0',
                '\n' => continue,
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    continue;
                }
                escape @ ('u' | 'x') => {
                    let mut scalar = 0u32;
                    if escape == 'u' && chars.peek() == Some(&'{') {
                        chars.next();
                        let mut digits = 0;
                        loop {
                            let digit = chars.next()?;
                            if digit == '}' {
                                if digits == 0 {
                                    return None;
                                }
                                break;
                            }
                            if digits == 6 {
                                return None;
                            }
                            scalar = scalar * 16 + digit.to_digit(16)?;
                            digits += 1;
                        }
                    } else {
                        for _ in 0..if escape == 'u' { 4 } else { 2 } {
                            scalar = scalar * 16 + chars.next()?.to_digit(16)?;
                        }
                    }
                    if (0xd800..=0xdbff).contains(&scalar) {
                        if chars.next()? != '\\' || chars.next()? != 'u' {
                            return None;
                        }
                        let mut low = 0u32;
                        for _ in 0..4 {
                            low = low * 16 + chars.next()?.to_digit(16)?;
                        }
                        scalar = 0x10000 + ((scalar - 0xd800) << 10) + low.checked_sub(0xdc00)?;
                    }
                    char::from_u32(scalar)?
                }
                other => other,
            }
        } else if quote == '`' && c == '$' && chars.peek() == Some(&'{') {
            chars.next();
            let mut name = String::new();
            for c in chars.by_ref().take_while(|c| *c != '}') {
                if !budget.character(&mut name, c) {
                    return None;
                }
            }
            if !budget.step(name.len()) {
                return None;
            }
            let text = match bindings.get(name.trim()) {
                Some(JsValue::Text(value)) => value,
                _ => "\0",
            };
            if !budget.append(&mut out, text) {
                return None;
            }
            continue;
        } else {
            c
        };
        if !budget.character(&mut out, decoded) {
            return None;
        }
    }
    Some(out)
}

fn js_split<'a>(
    tokens: &'a [JsToken<'a>],
    separator: char,
    budget: &mut JsBudget,
) -> Option<Vec<&'a [JsToken<'a>]>> {
    if !budget.step(tokens.len()) {
        return None;
    }
    let (mut depth, mut start) = (0i32, 0);
    let mut parts = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        match token {
            JsToken::Punct(c) if *c == separator && depth == 0 => {
                if !budget.items(parts.len() + 1)
                    || !budget.allocate(std::mem::size_of::<&[JsToken<'_>]>())
                {
                    return None;
                }
                parts.push(&tokens[start..i]);
                start = i + 1;
            }
            JsToken::Punct('(' | '[' | '{') => depth += 1,
            JsToken::Punct(')' | ']' | '}') => depth -= 1,
            _ => {}
        }
    }
    if !budget.items(parts.len() + 1) || !budget.allocate(std::mem::size_of::<&[JsToken<'_>]>()) {
        return None;
    }
    parts.push(&tokens[start..]);
    Some(parts)
}

fn js_value(
    tokens: &[JsToken<'_>],
    bindings: &JsBindings<'_>,
    budget: &mut JsBudget,
    depth: usize,
) -> JsValue {
    if !budget.depth(depth) || !budget.step(1) {
        return js_unknown();
    }
    // Assignment expressions are outside this static subset. Reject a chain
    // before scanning its suffix; walking a=b=c=... stays linear as well.
    if matches!(tokens, [JsToken::Word(_), JsToken::Punct('='), ..]) {
        return js_unknown();
    }
    let Some(parts) = js_split(tokens, '+', budget) else {
        return js_unknown();
    };
    if parts.len() > 1 {
        let mut text = String::new();
        for part in parts {
            let value = js_value(part, bindings, budget, depth + 1);
            let text_part = match &value {
                JsValue::Text(value) => value.as_str(),
                _ => "\0",
            };
            if !budget.append(&mut text, text_part) {
                return js_unknown();
            }
        }
        return JsValue::Text(text);
    }
    if let [base @ .., JsToken::Punct('.'), JsToken::Word("join"), JsToken::Punct('('), JsToken::Literal(separator), JsToken::Punct(')')] =
        tokens
    {
        if let (JsValue::Array(parts), Some(separator)) = (
            js_value(base, bindings, budget, depth + 1),
            js_literal(separator, bindings, budget),
        ) {
            let mut text = String::new();
            for (i, value) in parts.iter().enumerate() {
                if i > 0 && !budget.append(&mut text, &separator) {
                    return js_unknown();
                }
                let part = match value {
                    JsValue::Text(s) => s.as_str(),
                    _ => "\0",
                };
                if !budget.append(&mut text, part) {
                    return js_unknown();
                }
            }
            return JsValue::Text(text);
        }
    }
    if let [JsToken::Word("Object"), JsToken::Punct('.'), JsToken::Word("entries"), JsToken::Punct('('), inner @ .., JsToken::Punct(')')] =
        tokens
    {
        if let JsValue::Object(entries) = js_value(inner, bindings, budget, depth + 1) {
            if !budget.allocate(entries.len() * 3 * std::mem::size_of::<JsValue>()) {
                return js_unknown();
            }
            return JsValue::Array(
                entries
                    .into_iter()
                    .map(|(key, value)| JsValue::Array(vec![JsValue::Text(key), value]))
                    .collect(),
            );
        }
    }
    match tokens {
        [JsToken::Literal(raw)] => js_literal(raw, bindings, budget)
            .map(JsValue::Text)
            .unwrap_or_else(js_unknown),
        [JsToken::Word(name)] => {
            if !budget.step(name.len()) {
                return js_unknown();
            }
            bindings
                .get(name)
                .map(|v| js_copy(v, budget, depth))
                .unwrap_or_else(js_unknown)
        }
        [JsToken::Punct('['), inner @ .., JsToken::Punct(']')] => {
            let Some(parts) = js_split(inner, ',', budget) else {
                return js_unknown();
            };
            if !budget.allocate(parts.len() * std::mem::size_of::<JsValue>()) {
                return js_unknown();
            }
            let mut items = Vec::with_capacity(parts.len());
            for part in parts.into_iter().filter(|p| !p.is_empty()) {
                items.push(js_value(part, bindings, budget, depth + 1));
                if budget.exhausted {
                    return js_unknown();
                }
            }
            JsValue::Array(items)
        }
        [JsToken::Punct('{'), inner @ .., JsToken::Punct('}')] => {
            let Some(parts) = js_split(inner, ',', budget) else {
                return js_unknown();
            };
            if !budget.allocate(parts.len() * std::mem::size_of::<(String, JsValue)>()) {
                return js_unknown();
            }
            let mut entries = Vec::with_capacity(parts.len());
            for part in parts {
                let [key, JsToken::Punct(':'), value @ ..] = part else {
                    continue;
                };
                let key = match key {
                    JsToken::Literal(raw) => js_literal(raw, bindings, budget),
                    JsToken::Word(name) => budget.text(name),
                    _ => None,
                };
                let Some(key) = key else {
                    return js_unknown();
                };
                entries.push((key, js_value(value, bindings, budget, depth + 1)));
                if budget.exhausted {
                    return js_unknown();
                }
            }
            JsValue::Object(entries)
        }
        _ => js_unknown(),
    }
}

fn js_patch_calls<'a>(
    program: &JsProgram<'a>,
    scope: std::ops::Range<usize>,
    bindings: &mut JsBindings<'a>,
    patches: &mut Vec<String>,
    budget: &mut JsBudget,
    depth: usize,
) {
    if !budget.depth(depth) {
        return;
    }
    let tokens = &program.tokens[scope.clone()];
    let mut i = 0;
    while i < tokens.len() && budget.step(1) {
        if tokens.get(i..i + 2) == Some(&[JsToken::Word("for"), JsToken::Punct('(')]) {
            let end = program.end(&scope, i + 2, true);
            // A missing ')' cannot supply a body range. In particular end+1
            // would otherwise exceed this scope's token slice.
            if tokens.get(end) == Some(&JsToken::Punct(')')) && budget.step(end - (i + 2)) {
                if let Some(of) = tokens[i + 2..end]
                    .iter()
                    .position(|t| *t == JsToken::Word("of"))
                {
                    let of = i + 2 + of;
                    let mut names = Vec::new();
                    for token in &tokens[i + 2..of] {
                        if let JsToken::Word(name) = token {
                            if !matches!(*name, "const" | "let" | "var") {
                                if !budget.items(names.len() + 1)
                                    || !budget.allocate(std::mem::size_of::<&str>())
                                {
                                    return;
                                }
                                names.push(*name);
                            }
                        }
                    }
                    if !budget.allocate(names.len() * std::mem::size_of::<&str>()) {
                        return;
                    }
                    let mut shadowed = std::collections::HashSet::with_capacity(names.len());
                    for name in &names {
                        if !budget.step(name.len()) {
                            return;
                        }
                        shadowed.insert(*name);
                    }
                    if let JsValue::Array(values) =
                        js_value(&tokens[of + 1..end], bindings, budget, depth + 1)
                    {
                        let braced = tokens.get(end + 1) == Some(&JsToken::Punct('{'));
                        let start = end + if braced { 2 } else { 1 };
                        let finish = program.end(&scope, start, !braced);
                        for value in values {
                            if !budget.iteration() {
                                return;
                            }
                            let Some(mut local) = js_copy_bindings(bindings, budget, depth + 1)
                            else {
                                return;
                            };
                            if names.len() == 1 {
                                js_bind(&mut local, names[0], value, budget);
                            } else if let JsValue::Array(values) = value {
                                for (name, value) in names.iter().zip(values) {
                                    js_bind(&mut local, name, value, budget);
                                }
                            }
                            js_patch_calls(
                                program,
                                scope.start + start..scope.start + finish,
                                &mut local,
                                patches,
                                budget,
                                depth + 1,
                            );
                            for (name, value) in bindings.iter_mut() {
                                if !budget.step(name.len() + 1) {
                                    return;
                                }
                                if !shadowed.contains(*name) {
                                    if let Some(updated) = local.get(name) {
                                        *value = js_copy(updated, budget, depth + 1);
                                    }
                                }
                            }
                            if budget.exhausted {
                                return;
                            }
                        }
                        i = finish + 1;
                        continue;
                    }
                }
            }
        }
        if let (Some(JsToken::Word(name)), Some(JsToken::Punct('+')), Some(JsToken::Punct('='))) =
            (tokens.get(i), tokens.get(i + 1), tokens.get(i + 2))
        {
            let end = program.end(&scope, i + 3, true);
            let value = js_value(&tokens[i + 3..end], bindings, budget, depth + 1);
            if !budget.step(name.len()) {
                return;
            }
            if let Some(left) = bindings.get_mut(name) {
                if let (JsValue::Text(text), JsValue::Text(right)) = (&mut *left, value) {
                    if !budget.append(text, &right) {
                        *left = js_unknown();
                    }
                }
            }
        }
        if let (Some(JsToken::Word(name)), Some(JsToken::Punct('='))) =
            (tokens.get(i), tokens.get(i + 1))
        {
            if tokens.get(i + 2) != Some(&JsToken::Punct('=')) {
                let end = program.end(&scope, i + 2, true);
                let value = js_value(&tokens[i + 2..end], bindings, budget, depth + 1);
                js_bind(bindings, name, value, budget);
            }
        }
        if tokens.get(i..i + 4)
            == Some(&[
                JsToken::Word("tools"),
                JsToken::Punct('.'),
                JsToken::Word("apply_patch"),
                JsToken::Punct('('),
            ])
        {
            let end = program.end(&scope, i + 4, true);
            if let JsValue::Text(patch) = js_value(&tokens[i + 4..end], bindings, budget, depth + 1)
            {
                if !budget.step(patch.len()) {
                    return;
                }
                if !patch
                    .lines()
                    .any(|line| line.starts_with("*** ") && line.contains('\0'))
                {
                    if !budget.allocate(std::mem::size_of::<String>()) {
                        return;
                    }
                    patches.push(patch);
                }
            }
        }
        i += 1;
    }
}

fn exec_patches_bounded(code: &str, budget: &mut JsBudget) -> Vec<String> {
    let mut patches = Vec::new();
    if let Some(program) = JsProgram::new(code, budget) {
        js_patch_calls(
            &program,
            0..program.tokens.len(),
            &mut JsBindings::new(),
            &mut patches,
            budget,
            0,
        );
    }
    patches
}

fn exec_patches(code: &str) -> Vec<String> {
    exec_patches_bounded(code, &mut JsBudget::default())
}

fn field<'a>(value: Option<&'a Value>, names: &[&str]) -> Option<&'a str> {
    let value = value?;
    names
        .iter()
        .find_map(|name| value.get(name).and_then(Value::as_str))
}

fn patch_paths(patch: &str, paths: &mut Vec<(String, &'static str)>) {
    let mut update = None;
    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("*** Move to: ") {
            if let Some(old) = update.take() {
                paths.push((old, "delete"));
                paths.push((path.to_owned(), "change"));
            }
        } else if line.starts_with("*** Add File: ")
            || line.starts_with("*** Update File: ")
            || line.starts_with("*** Delete File: ")
            || line == "*** End Patch"
        {
            if let Some(path) = update.take() {
                paths.push((path, "change"));
            }
            if let Some(path) = line.strip_prefix("*** Add File: ") {
                paths.push((path.to_owned(), "change"));
            } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
                paths.push((path.to_owned(), "delete"));
            } else if let Some(path) = line.strip_prefix("*** Update File: ") {
                update = Some(path.to_owned());
            }
        }
    }
    if let Some(path) = update {
        paths.push((path, "change"));
    }
}

/// Pure lexical resolution: no canonicalize, stat, or symlink traversal.
fn resolve(path: &str, cwd: &str) -> String {
    if cwd.is_empty() {
        return path.to_owned();
    }
    // Transcripts can come from another OS. Interpret the source path syntax,
    // not the host's Path::components rules (notably POSIX paths on Windows).
    let windows_drive =
        |s: &str| matches!(s.as_bytes(), [d, b':', b'/' | b'\\', ..] if d.is_ascii_alphabetic());
    let drive_relative =
        matches!(path.as_bytes(), [d, b':', ..] if d.is_ascii_alphabetic()) && !windows_drive(path);
    let path = if drive_relative {
        if windows_drive(cwd) && path[..1].eq_ignore_ascii_case(&cwd[..1]) {
            &path[2..]
        } else {
            // Another drive's current directory is not supplied by this session.
            return path.to_owned();
        }
    } else {
        path
    };
    let windows = windows_drive(path)
        || path.starts_with("\\\\")
        || windows_drive(cwd)
        || cwd.starts_with("\\\\");
    let (path, cwd) = if windows {
        (path.replace('\\', "/"), cwd.replace('\\', "/"))
    } else {
        (path.to_owned(), cwd.to_owned())
    };
    let joined = if windows && path.starts_with('/') && !path.starts_with("//") {
        // A rooted Windows operand keeps the cwd's drive or UNC share.
        let root = if windows_drive(&cwd) {
            &cwd[..2]
        } else {
            let end = cwd.match_indices('/').nth(3).map_or(cwd.len(), |(i, _)| i);
            &cwd[..end]
        };
        format!("{root}{path}")
    } else if path.starts_with('/') || windows_drive(&path) {
        path
    } else {
        format!("{cwd}/{path}")
    };
    let (prefix, rest, floor) = if windows_drive(&joined) {
        (format!("{}/", &joined[..2]), &joined[3..], 0)
    } else if windows && joined.starts_with("//") {
        ("//".to_owned(), &joined[2..], 2)
    } else if let Some(rest) = joined.strip_prefix('/') {
        ("/".to_owned(), rest, 0)
    } else {
        (String::new(), joined.as_str(), 0)
    };
    let mut parts = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.len() > floor && parts.last() != Some(&"..") => {
                parts.pop();
            }
            ".." if prefix.is_empty() => parts.push(part),
            ".." => {}
            _ => parts.push(part),
        }
    }
    let result = format!("{prefix}{}", parts.join("/"));
    if windows {
        result.replace('/', "\\")
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::parse_utils::tool_call_view;
    use serde_json::json;

    fn files(agent: AgentId, name: &str, input: Value) -> Vec<ToolFile> {
        let call = tool_call_view("call".into(), name, &input, None, false);
        from_call(agent, &call, 3, "/home/alice/project")
    }

    #[test]
    fn explicit_file_tools_and_encoded_antigravity_paths() {
        for (agent, name, input, op) in [
            (
                AgentId::ClaudeCode,
                "Edit",
                json!({"file_path":"src/../edit.rs"}),
                "change",
            ),
            (
                AgentId::ClaudeCode,
                "Write",
                json!({"file_path":"edit.rs"}),
                "change",
            ),
            (
                AgentId::ClaudeCode,
                "Read",
                json!({"file_path":"edit.rs"}),
                "read",
            ),
            (
                AgentId::Antigravity,
                "view_file",
                json!({"AbsolutePath":"\"/home/alice/project/edit.rs\""}),
                "read",
            ),
            (
                AgentId::Antigravity,
                "client_edit_file",
                json!({"target_file":"\"edit.rs\""}),
                "change",
            ),
            (
                AgentId::Antigravity,
                "write_to_file",
                json!({"TargetFile":"edit.rs"}),
                "change",
            ),
            (
                AgentId::Antigravity,
                "read_file",
                json!({"Path":"\"edit.rs\""}),
                "read",
            ),
            (
                AgentId::Hermes,
                "patch",
                json!({"path":"edit.rs", "old_string":"a", "new_string":"b"}),
                "change",
            ),
        ] {
            assert_eq!(
                files(agent, name, input),
                vec![ToolFile {
                    idx: 3,
                    path: "/home/alice/project/edit.rs".into(),
                    op
                }]
            );
        }
    }

    #[test]
    fn patch_collects_all_operations_before_clipping() {
        let patch = format!("*** Begin Patch\n*** Add File: added.rs\n+{}\n*** Update File: old.rs\n*** Move to: moved.rs\n@@\n-a\n+b\n*** Delete File: deleted.rs\n*** Update File: last.rs\n@@\n-a\n+b\n*** End Patch", "x".repeat(17 * 1024));
        let result = files(AgentId::Codex, "apply_patch", json!(patch));
        let pairs: Vec<_> = result.iter().map(|f| (f.path.as_str(), f.op)).collect();
        assert_eq!(
            pairs,
            vec![
                ("/home/alice/project/added.rs", "change"),
                ("/home/alice/project/old.rs", "delete"),
                ("/home/alice/project/moved.rs", "change"),
                ("/home/alice/project/deleted.rs", "delete"),
                ("/home/alice/project/last.rs", "change"),
            ]
        );
        assert_eq!(
            files(AgentId::Hermes, "patch", json!({"patch":patch})),
            result
        );
    }

    #[test]
    fn shell_and_resource_tools_do_not_imply_file_operations() {
        for name in [
            "Bash",
            "bash",
            "exec",
            "exec_command",
            "terminal",
            "run_command",
            "mcp__example__write_file",
            "search_files",
        ] {
            assert!(files(
                AgentId::ClaudeCode,
                name,
                json!({"path":"edit.rs", "file_path":"edit.rs", "command":"cat edit.rs > new.rs"})
            )
            .is_empty());
        }
    }

    #[test]
    fn paths_without_cwd_are_kept_and_relative_paths_are_lexical() {
        assert_eq!(resolve("./src/../file.rs", ""), "./src/../file.rs");
        assert_eq!(
            resolve("../../../../file.rs", "/home/alice/project"),
            "/file.rs"
        );
        assert_eq!(
            resolve("../file.rs", "/home/alice/project"),
            "/home/alice/file.rs"
        );
    }
    #[test]
    fn source_windows_paths_are_resolved_on_any_host() {
        assert_eq!(
            resolve(r"src\..\file.rs", r"C:\Users\alice\project"),
            r"C:\Users\alice\project\file.rs"
        );
        assert_eq!(
            resolve(r"D:\files\..\file.rs", r"C:\Users\alice"),
            r"D:\file.rs"
        );
        assert_eq!(
            resolve(r"..\file.rs", r"\\server\share\project"),
            r"\\server\share\file.rs"
        );
    }
    #[test]
    fn windows_root_relative_paths_inherit_the_cwd_volume() {
        assert_eq!(
            resolve(r"\tmp\alice.txt", r"C:\Users\alice\project"),
            r"C:\tmp\alice.txt"
        );
        assert_eq!(
            resolve("/tmp/alice.txt", r"C:\Users\alice\project"),
            r"C:\tmp\alice.txt"
        );
        assert_eq!(
            resolve(r"\tmp\alice.txt", r"\\server\share\project"),
            r"\\server\share\tmp\alice.txt"
        );
    }
    #[test]
    fn windows_drive_relative_paths_use_only_the_known_drive_cwd() {
        let cwd = r"C:\Users\alice\project";
        assert_eq!(
            resolve("C:alice.txt", cwd),
            r"C:\Users\alice\project\alice.txt"
        );
        assert_eq!(resolve(r"c:..\alice.txt", cwd), r"C:\Users\alice\alice.txt");
        assert_eq!(resolve("C:", cwd), cwd);
        assert_eq!(resolve("D:alice.txt", cwd), "D:alice.txt");
        assert_eq!(resolve("C:alice.txt", ""), "C:alice.txt");
        assert_eq!(resolve("C:alice.txt", "/home/alice/project"), "C:alice.txt");
    }
    #[test]
    fn source_specific_file_names_and_arguments_are_recognized() {
        for (agent, name, input, op) in [
            (AgentId::Kimi, "ReadFile", json!({"path":"file.rs"}), "read"),
            (
                AgentId::Opencode,
                "edit",
                json!({"filePath":"file.rs"}),
                "change",
            ),
            (
                AgentId::Opencode,
                "read",
                json!({"filePath":"file.rs"}),
                "read",
            ),
            (
                AgentId::ClaudeCode,
                "NotebookEdit",
                json!({"notebook_path":"file.rs"}),
                "change",
            ),
        ] {
            assert_eq!(
                files(agent, name, input),
                vec![ToolFile {
                    idx: 3,
                    path: "/home/alice/project/file.rs".into(),
                    op
                }]
            );
        }
    }
    #[test]
    fn code_mode_patches_support_literals_bindings_templates_and_batches() {
        let code = r#"
            const file = "template.rs";
            const a = "*** Begin Patch\n*** Add File: first.rs\n+x\n*** End Patch";
            const b = `*** Begin Patch
*** Update File: ${file}
@@
-a
+b
*** End Patch`;
            for (const patch of [a, b]) { const ignored = 1; text(await tools.apply_patch(patch)); }
            tools.apply_patch('*** Begin Patch\n*** Delete File: old.rs\n*** End Patch');
            // tools.apply_patch("*** Begin Patch\n*** Add File: comment.rs\n+x\n*** End Patch")
            const example = "tools.apply_patch(\"*** Add File: example.rs\")";
        "#;
        let files = files(AgentId::Codex, "exec", json!(code));
        assert_eq!(
            files
                .iter()
                .map(|f| (f.path.as_str(), f.op))
                .collect::<Vec<_>>(),
            [
                ("/home/alice/project/first.rs", "change"),
                ("/home/alice/project/template.rs", "change"),
                ("/home/alice/project/old.rs", "delete"),
            ]
        );
    }

    #[test]
    fn code_mode_static_headers_survive_dynamic_body_expressions() {
        let code = r#"
            const patch = "*** Begin Patch\n*** Add File: generated.rs\n" + content.split("\n").map(line => "+" + line).join("\n") + "\n*** End Patch";
            tools.apply_patch(patch);
            tools.apply_patch("*** Begin Patch\n*** Add File: " + unknownPath + "\n+x\n*** End Patch");
        "#;
        let found = files(AgentId::Codex, "exec", json!(code));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "/home/alice/project/generated.rs");
    }

    #[test]
    fn code_mode_string_escapes_preserve_file_names() {
        let code = r#"tools.apply_patch("*** Begin Patch\n*** Add File: \u0061\"b\\c.rs\n+x\n*** End Patch");"#;
        let found = files(AgentId::Codex, "exec", json!(code));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "/home/alice/project/a\"b\\c.rs");
    }
    #[test]
    fn code_mode_array_join_keeps_static_headers_around_generated_lines() {
        let code = r#"const files = ["a.rs", "b.rs"];
            for (const file of files) {
                const patch = ["*** Begin Patch", `*** Update File: ${file}`, "@@", ...lines.map(l => "+" + l), "*** End Patch"].join("\n");
                tools.apply_patch(patch);
            }"#;
        let found = files(AgentId::Codex, "exec", json!(code));
        assert_eq!(
            found.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            ["/home/alice/project/a.rs", "/home/alice/project/b.rs"]
        );
    }
    #[test]
    fn code_mode_builders_accumulate_headers_over_static_file_sets() {
        let code = r#"const changes = {"a.rs": replacementA, "b.rs": replacementB};
            let patch = "*** Begin Patch\n";
            for (const [file, change] of Object.entries(changes)) {
                patch += `*** Update File: ${file}\n@@\n`;
                patch += change.map(line => "+" + line).join("\n") + "\n";
            }
            patch += "*** End Patch";
            tools.apply_patch(patch);"#;
        let found = files(AgentId::Codex, "exec", json!(code));
        assert_eq!(
            found.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            ["/home/alice/project/a.rs", "/home/alice/project/b.rs"]
        );
    }
    mod budget_cases {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/code_mode_budget/cases.rs"
        ));
    }

    fn bounded_attack(name: &'static str) {
        assert!(budget_cases::ATTACKS.contains(&name));
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let script = budget_cases::script(name);
                let mut budget = JsBudget::default();
                let patches = exec_patches_bounded(&script, &mut budget);
                assert!(
                    patches
                        .iter()
                        .any(|patch| patch.contains("/home/alice/before.rs")),
                    "{name}"
                );
                assert!(budget.steps <= JS_MAX_STEPS, "{name}");
                assert!(budget.iterations <= JS_MAX_ITERATIONS, "{name}");
                assert!(budget.bytes <= JS_MAX_BYTES, "{name}");
                assert!(budget.max_depth <= JS_MAX_DEPTH, "{name}");
                if name == "assignment_chain" {
                    assert!(budget.steps <= 20 * 50_000 + 1024, "{}", budget.steps);
                } else if name != "missing_for_close" {
                    assert!(budget.limited, "{name}");
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    macro_rules! budget_regressions {
        ($($name:ident),+ $(,)?) => {$ (
            #[test]
            fn $name() { bounded_attack(stringify!($name)); }
        )+};
    }
    budget_regressions!(
        missing_for_close,
        append_doubling,
        expression_doubling,
        array_doubling,
        nested_loops,
        arrays_20000,
        arrays_5000,
        arrays_200000,
        objects_200000,
        assignment_chain
    );

    #[test]
    fn every_byte_truncation_of_code_mode_fixtures_is_safe() {
        for script in [
            include_str!("../../tests/fixtures/code_mode_budget/literals.js"),
            include_str!("../../tests/fixtures/code_mode_budget/batches.js"),
            include_str!("../../tests/fixtures/code_mode_budget/builder.js"),
        ] {
            assert!(!files(AgentId::Codex, "exec", json!(script)).is_empty());
            for end in 0..=script.len() {
                let prefix = String::from_utf8_lossy(&script.as_bytes()[..end]);
                files(AgentId::Codex, "exec", json!(prefix));
            }
        }
    }

    #[test]
    fn string_array_and_input_caps_are_enforced_without_erasing_prior_paths() {
        let mut budget = JsBudget::default();
        let mut text = String::new();
        assert!(budget.append(&mut text, &"x".repeat(JS_MAX_STRING)));
        assert!(!budget.append(&mut text, "x"));
        assert_eq!(text.len(), JS_MAX_STRING);
        assert!(budget.limited);

        let code = format!("const a=[{}];", "'x',".repeat(JS_MAX_ITEMS + 1));
        let mut budget = JsBudget::default();
        exec_patches_bounded(&code, &mut budget);
        assert!(budget.limited);

        let mut code = budget_cases::script("missing_for_close");
        code.push_str("/*");
        code.push_str(&"x".repeat(JS_MAX_INPUT));
        let mut budget = JsBudget::default();
        let patches = exec_patches_bounded(&code, &mut budget);
        assert!(budget.limited);
        assert!(patches.iter().any(|p| p.contains("before.rs")));
    }

    #[test]
    fn cached_expression_ends_match_linear_scans_for_all_fixture_prefixes() {
        fn scan(tokens: &[JsToken<'_>], start: usize, statement: bool) -> usize {
            let mut depth = 0;
            for (i, t) in tokens.iter().enumerate().skip(start) {
                match t {
                    JsToken::Punct(')' | ']' | '}') if depth == 0 => return i,
                    JsToken::Punct(',' | ';') if depth == 0 && statement => return i,
                    JsToken::Punct('(' | '[' | '{') => depth += 1,
                    JsToken::Punct(')' | ']' | '}') => depth -= 1,
                    _ => {}
                }
            }
            tokens.len()
        }
        for code in [
            include_str!("../../tests/fixtures/code_mode_budget/batches.js"),
            include_str!("../../tests/fixtures/code_mode_budget/builder.js"),
        ] {
            for end in code
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(code.len()))
            {
                let mut budget = JsBudget::default();
                let program = JsProgram::new(&code[..end], &mut budget).unwrap();
                for start in 0..=program.tokens.len() {
                    for statement in [false, true] {
                        assert_eq!(
                            program.end(&(0..program.tokens.len()), start, statement),
                            scan(&program.tokens, start, statement)
                        );
                    }
                }
            }
        }
    }
}
