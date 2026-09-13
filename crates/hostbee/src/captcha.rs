//! 显式登录的人工 CAPTCHA：图片落盘、Kitty 显示及一次性凭证交换。
use crate::{
    error::CliError,
    gql::{self, GraphqlTransport},
};
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};

pub fn required(error: &CliError) -> bool {
    matches!(error, CliError::GraphQlErrors(errors) if errors.iter().any(|e| e.pointer("/extensions/code").and_then(Value::as_str) == Some("captcha.id_required")))
}

/// 获取图片、交给用户解答，返回已验证的凭证；不重试、不保存答案。
pub fn solve(
    transport: &impl GraphqlTransport,
    endpoint: &str,
    read_answer: impl FnOnce() -> Result<String, CliError>,
) -> Result<String, CliError> {
    let data = request(
        transport,
        endpoint,
        "mutation { generateCaptcha { captchaId imageBase64 } }",
        None,
    )?;
    let challenge = &data["generateCaptcha"];
    let id = nonempty(&challenge["captchaId"])?;
    let image = nonempty(&challenge["imageBase64"])?;
    let directory = tempfile::Builder::new()
        .prefix("hostbee-captcha-")
        .tempdir()
        .map_err(|e| CliError::Input(e.to_string()))?;
    let path = save_image(image, directory.path())?;
    eprintln!("验证码图片：{}", path.display());
    let kitty = std::io::stderr().is_terminal()
        && (std::env::var("TERM").is_ok_and(|v| v == "xterm-kitty")
            || std::env::var_os("KITTY_WINDOW_ID").is_some())
        && std::env::var_os("TMUX").is_none()
        && std::env::var_os("STY").is_none();
    if kitty {
        // 文件始终可用；终端显示失败不妨碍用户手动打开图片。
        if let Err(e) = render_kitty(&path, &mut std::io::stderr()) {
            eprintln!("终端显示失败，请打开图片：{}", e.message());
        }
    }
    let answer = read_answer();
    // stdin 被重定向或 EOF 时没有终端回显换行，仍须让错误 JSON 独占一行。
    eprintln!();
    let answer = answer?;
    if answer.trim().is_empty() {
        return Err(CliError::Input("验证码不能为空".into()));
    }
    let data = request(
        transport,
        endpoint,
        "mutation($input: VerifyCaptchaInput!) { verifyCaptcha(input: $input) { captchaId } }",
        Some(json!({"input":{"captchaId":id,"answer":answer.trim()}})),
    )?;
    Ok(nonempty(&data["verifyCaptcha"]["captchaId"])?.to_owned())
}

fn nonempty(value: &Value) -> Result<&str, CliError> {
    value
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| CliError::InvalidResponse("CAPTCHA 响应缺少有效字段".into()))
}
fn request(
    transport: &impl GraphqlTransport,
    endpoint: &str,
    document: &str,
    variables: Option<Value>,
) -> Result<Value, CliError> {
    let (status, text) = transport
        .post(endpoint, None, &gql::request_body(document, variables))
        .map_err(CliError::Transport)?;
    gql::parse_response(status, &text)
}

/// 将后端 data URL 保存为图片；只支持后端使用的 JPEG 和 PNG。
pub fn save_image(
    data_url: &str,
    directory: &std::path::Path,
) -> Result<std::path::PathBuf, CliError> {
    let (encoded, extension) = if let Some(s) = data_url.strip_prefix("data:image/jpeg;base64,") {
        (s, "jpg")
    } else if let Some(s) = data_url.strip_prefix("data:image/png;base64,") {
        (s, "png")
    } else {
        return Err(CliError::InvalidResponse(
            "CAPTCHA 图片格式必须为 JPEG 或 PNG data URL".into(),
        ));
    };
    let bytes = data_encoding::BASE64
        .decode(encoded.as_bytes())
        .map_err(|_| CliError::InvalidResponse("CAPTCHA 图片 Base64 无效".into()))?;
    if bytes.is_empty() {
        return Err(CliError::InvalidResponse("CAPTCHA 图片为空".into()));
    }
    image::load_from_memory(&bytes)
        .map_err(|_| CliError::InvalidResponse("CAPTCHA 图片内容无效".into()))?;
    let path = directory.join(format!("captcha.{extension}"));
    std::fs::write(&path, bytes)
        .map_err(|e| CliError::Input(format!("保存验证码图片失败: {e}")))?;
    Ok(path)
}

/// 使用 Kitty 直接传输 PNG，响应静默，避免协议回复污染用户输入。
pub fn render_kitty(path: &std::path::Path, output: &mut impl Write) -> Result<(), CliError> {
    let image = image::open(path)
        .map_err(|e| CliError::InvalidResponse(format!("读取验证码图片失败: {e}")))?;
    let mut png = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| CliError::InvalidResponse(e.to_string()))?;
    let encoded = data_encoding::BASE64.encode(png.get_ref());
    let count = encoded.len().div_ceil(4096);
    for (i, chunk) in encoded.as_bytes().chunks(4096).enumerate() {
        let more = usize::from(i + 1 < count);
        let control = if i == 0 {
            format!("a=T,f=100,t=d,q=2,m={more}")
        } else {
            format!("m={more}")
        };
        output
            .write_all(format!("\x1b_G{control};").as_bytes())
            .and_then(|_| output.write_all(chunk))
            .and_then(|_| output.write_all(b"\x1b\\"))
            .map_err(|e| CliError::Input(e.to_string()))?;
    }
    output
        .write_all(b"\n")
        .and_then(|_| output.flush())
        .map_err(|e| CliError::Input(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_保存后_kitty_输出可还原为同尺寸_png() {
        let pixels = image::RgbImage::from_fn(200, 60, |x, y| {
            image::Rgb([
                (x * 13 + y * 7) as u8,
                (x * 3 + y * 19) as u8,
                (x * 17 + y * 11) as u8,
            ])
        });
        let mut jpeg = std::io::Cursor::new(Vec::new());
        pixels
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = save_image(
            &format!(
                "data:image/jpeg;base64,{}",
                data_encoding::BASE64.encode(jpeg.get_ref())
            ),
            dir.path(),
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), *jpeg.get_ref());
        let mut output = Vec::new();
        render_kitty(&path, &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.starts_with("\x1b_Ga=T,f=100,t=d,q=2,m=1;"));
        let mut encoded = String::new();
        let frames: Vec<_> = text
            .trim_end_matches('\n')
            .split("\x1b\\")
            .filter(|s| !s.is_empty())
            .collect();
        assert!(frames.len() > 1);
        for (i, frame) in frames.iter().enumerate() {
            let (control, payload) = frame.split_once(';').unwrap();
            assert!(payload.len() <= 4096);
            assert_eq!(payload.len() % 4, 0);
            assert!(control.ends_with(if i + 1 == frames.len() { "m=0" } else { "m=1" }));
            encoded.push_str(payload);
        }
        let png = data_encoding::BASE64.decode(encoded.as_bytes()).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (200, 60));
        assert_eq!(decoded.to_rgb8(), image::open(path).unwrap().to_rgb8());
    }

    #[test]
    fn 图片保存拒绝损坏内容() {
        let dir = tempfile::tempdir().unwrap();
        for input in [
            "data:image/jpeg;base64,aGVsbG8=",
            "data:image/png;base64,",
            "data:image/png;base64,!",
            "https://example.com/captcha.png",
        ] {
            assert!(save_image(input, dir.path()).is_err(), "应拒绝损坏图片");
        }
    }
}
