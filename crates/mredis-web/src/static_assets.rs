//! 静态资源：`web/` 下的三个文件在编译期内嵌进二进制。

pub const INDEX_HTML: &str = include_str!("../../../web/index.html");
pub const APP_JS: &str = include_str!("../../../web/app.js");
pub const STYLE_CSS: &str = include_str!("../../../web/style.css");
