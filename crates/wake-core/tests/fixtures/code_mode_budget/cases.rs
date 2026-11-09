pub const ATTACKS: &[&str] = &[
    "missing_for_close",
    "append_doubling",
    "expression_doubling",
    "array_doubling",
    "nested_loops",
    "arrays_20000",
    "arrays_5000",
    "arrays_200000",
    "objects_200000",
    "assignment_chain",
];

pub fn script(name: &str) -> String {
    let mut script = String::from(
        r#"tools.apply_patch("*** Begin Patch\n*** Add File: /home/alice/before.rs\n+ok\n*** End Patch");"#,
    );
    match name {
        "missing_for_close" => script.push_str(r#"for (const f of ["a.rs"]"#),
        "append_doubling" => {
            script.push_str("let p='x';");
            script.push_str(&"p += p;".repeat(34));
            script.push_str("tools.apply_patch(p);");
        }
        "expression_doubling" => {
            script.push_str("let p='x';");
            script.push_str(&"p = p + p;".repeat(34));
            script.push_str("tools.apply_patch(p);");
        }
        "array_doubling" => {
            script.push_str("let a='x';");
            script.push_str(&"a = [a, a];".repeat(34));
            script.push_str("tools.apply_patch(a);");
        }
        "nested_loops" => {
            for i in 0..9 {
                script.push_str(&format!(
                    "for (const f{i} of ['0','1','2','3','4','5','6','7','8','9']) {{"
                ));
            }
            script.push_str(r#"tools.apply_patch("*** Begin Patch\n*** Add File: /home/alice/loop.rs\n+x\n*** End Patch");"#);
            script.push_str(&"}".repeat(9));
        }
        "arrays_20000" | "arrays_5000" | "arrays_200000" => {
            let n = match name {
                "arrays_5000" => 5_000,
                "arrays_20000" => 20_000,
                _ => 200_000,
            };
            script.push_str("const a=");
            script.push_str(&"[".repeat(n));
            script.push_str("'x'");
            script.push_str(&"]".repeat(n));
            script.push(';');
        }
        "objects_200000" => {
            script.push_str("const a=");
            script.push_str(&"{x:".repeat(200_000));
            script.push_str("'x'");
            script.push_str(&"}".repeat(200_000));
            script.push(';');
        }
        "assignment_chain" => {
            for i in 0..50_000 {
                script.push_str(&format!("v{i}="));
            }
            script.push_str("'x';");
        }
        _ => panic!("unknown synthetic case"),
    }
    script
}
