//! 写 XML 格式的属性列表（Info.plist）。只需要写，不需要读，几十行就够了

use std::fmt::Write;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    String(String),
    Bool(bool),
    Integer(i64),
    Real(f64),
    Array(Vec<Value>),
    Dict(Dict),
}

/// 保持插入顺序的字典；同一个键再设一次时覆盖原来的值
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dict(pub Vec<(String, Value)>);

impl Dict {
    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        let value = value.into();
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => entry.1 = value,
            None => self.0.push((key.to_owned(), value)),
        }
    }

    pub fn to_xml(&self) -> String {
        let mut out = String::from(concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
            "<plist version=\"1.0\">\n",
        ));
        write_value(&mut out, &Value::Dict(self.clone()), 0);
        out.push_str("</plist>\n");
        out
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<Vec<String>> for Value {
    fn from(items: Vec<String>) -> Self {
        Value::Array(items.into_iter().map(Value::String).collect())
    }
}

/// Cargo.toml 里 `info-plist` 表的值（经 `cargo metadata` 变成 JSON）转成属性列表的值；`null` 没有对应，返回 `None`
pub fn from_json(value: &serde_json::Value) -> Option<Value> {
    Some(match value {
        serde_json::Value::Null => return None,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Integer(i),
            None => Value::Real(n.as_f64()?),
        },
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(items) => Value::Array(items.iter().filter_map(from_json).collect()),
        serde_json::Value::Object(map) => Value::Dict(Dict(map.iter().filter_map(|(k, v)| Some((k.clone(), from_json(v)?))).collect())),
    })
}

fn write_value(out: &mut String, value: &Value, depth: usize) {
    let indent = "\t".repeat(depth);
    match value {
        Value::String(s) => {
            let _ = writeln!(out, "{indent}<string>{}</string>", escape(s));
        }
        Value::Bool(b) => {
            let _ = writeln!(out, "{indent}<{}/>", if *b { "true" } else { "false" });
        }
        Value::Integer(i) => {
            let _ = writeln!(out, "{indent}<integer>{i}</integer>");
        }
        Value::Real(r) => {
            let _ = writeln!(out, "{indent}<real>{r}</real>");
        }
        Value::Array(items) => {
            let _ = writeln!(out, "{indent}<array>");
            for item in items {
                write_value(out, item, depth + 1);
            }
            let _ = writeln!(out, "{indent}</array>");
        }
        Value::Dict(dict) => {
            let _ = writeln!(out, "{indent}<dict>");
            for (key, value) in &dict.0 {
                let _ = writeln!(out, "{indent}\t<key>{}</key>", escape(key));
                write_value(out, value, depth + 1);
            }
            let _ = writeln!(out, "{indent}</dict>");
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
