//! 前端资源直接编译进二进制，运行时无需额外目录。

pub const INDEX_HTML: &str = include_str!("../web/index.html");
pub const APP_JS: &str = include_str!("../web/app.js");
pub const LIVE_JS: &str = include_str!("../web/live.js");
pub const STYLES_CSS: &str = include_str!("../web/styles.css");
