//! 应用给的地址整理成 CEF 能打开的样子。只补明显缺的部分，写了协议的地址原样不动。

/// 整理 `input`：去掉两头的空白；空的打开空白页；文件的绝对路径前面加 `file://`；
/// 没写协议的（`example.com`、`localhost:8080/a`、`//example.com`）补上：本机的补 `http://`（本机开发用的服务器一般不带证书），
/// 别的补 `https://`。写了协议的（`https:`、`file:`、`data:`、`about:`……）原样返回
pub(crate) fn fix(input: &str) -> String {
    let url = input.trim();
    if url.is_empty() {
        return "about:blank".to_owned();
    }
    if let Some(rest) = url.strip_prefix("//") {
        return format!("{}//{rest}", default_scheme(rest));
    }
    if url.starts_with('/') {
        return format!("file://{url}");
    }
    if url.starts_with("\\\\") {
        // Windows 的网络路径 \\server\share\a.html
        return format!("file:{}", url.replace('\\', "/"));
    }
    if is_drive_path(url) {
        return format!("file:///{}", url.replace('\\', "/"));
    }
    if has_scheme(url) {
        return url.to_owned();
    }
    format!("{}//{url}", default_scheme(url))
}

/// `C:\a.html`、`C:/a.html`：盘符后面紧跟斜杠
fn is_drive_path(url: &str) -> bool {
    let bytes = url.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/')
}

/// 开头是 `协议:`。`localhost:8080`、`example.com:443/a` 冒号后面是端口，不算协议
fn has_scheme(url: &str) -> bool {
    let Some(colon) = url.find(':') else { return false };
    let (scheme, rest) = (&url[..colon], &url[colon + 1..]);
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid && !starts_with_port(rest)
}

/// 冒号后面是 1 到 5 位数字，接着是结尾或者路径、查询、片段
fn starts_with_port(rest: &str) -> bool {
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    (1..=5).contains(&digits) && matches!(rest.as_bytes().get(digits), None | Some(b'/' | b'?' | b'#'))
}

/// 没写协议的地址补哪个：本机的 `http:`，别的 `https:`
fn default_scheme(url: &str) -> &'static str {
    if is_local(host(url)) { "http:" } else { "https:" }
}

/// 地址里的主机名（不带端口），小写前的原样
fn host(url: &str) -> &str {
    let end = url.find(['/', '?', '#']).unwrap_or(url.len());
    let authority = &url[..end];
    // user:pass@host 的 @ 前面不是主机
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    if authority.starts_with('[') {
        // IPv6：[::1]:8080
        return authority.find(']').map_or(authority, |close| &authority[..=close]);
    }
    authority.split_once(':').map_or(authority, |(host, _)| host)
}

/// 本机：`localhost`、`*.localhost`、`127.*`、`[::1]`、`0.0.0.0`
fn is_local(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost") || host.starts_with("127.") || host == "[::1]" || host == "0.0.0.0"
}
