//! HTTPS 支持。
//!
//! 手机浏览器调用摄像头必须在**安全上下文**下（https 或 localhost），
//! 纯 http 的局域网地址在 Chrome 上 `navigator.mediaDevices` 直接是 undefined、
//! iOS Safari 也会拒绝，所以这里提供自签证书的自动生成与复用。

use std::path::{Path, PathBuf};

use anyhow::Context;

/// 证书与私钥路径
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// 准备证书：
/// * 用户通过 `--tls-cert/--tls-key` 提供则直接使用；
/// * 否则在数据目录生成自签证书并复用，本机 IP 变化导致 SAN 不匹配时会重新生成。
pub fn prepare(
    data_dir: &Path,
    cert: Option<&Path>,
    key: Option<&Path>,
    extra_hosts: &[String],
) -> anyhow::Result<TlsFiles> {
    if let (Some(cert), Some(key)) = (cert, key) {
        return Ok(TlsFiles {
            cert: cert.to_path_buf(),
            key: key.to_path_buf(),
        });
    }
    if cert.is_some() != key.is_some() {
        anyhow::bail!("--tls-cert 与 --tls-key 需要同时提供");
    }

    let cert_path = data_dir.join("tls-cert.pem");
    let key_path = data_dir.join("tls-key.pem");
    let san_path = data_dir.join("tls-san.txt");

    let mut hosts: Vec<String> = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    // 0.0.0.0 / :: 这类通配地址放进 SAN 没有意义，直接跳过
    hosts.extend(
        extra_hosts
            .iter()
            .filter(|host| match host.parse::<std::net::IpAddr>() {
                Ok(ip) => !ip.is_unspecified(),
                Err(_) => !host.trim().is_empty(),
            })
            .cloned(),
    );
    if let Some(ip) = crate::net::primary_local_ip() {
        hosts.push(ip.to_string());
    }
    hosts.sort();
    hosts.dedup();

    let want = hosts.join(",");
    let have = std::fs::read_to_string(&san_path).unwrap_or_default();
    if cert_path.exists() && key_path.exists() && have.trim() == want {
        return Ok(TlsFiles {
            cert: cert_path,
            key: key_path,
        });
    }

    generate(&cert_path, &key_path, &hosts)?;
    let _ = std::fs::write(&san_path, &want);
    tracing::info!(
        "已生成自签证书（SAN: {}），手机首次访问需要手动信任",
        hosts.join(", ")
    );
    Ok(TlsFiles {
        cert: cert_path,
        key: key_path,
    })
}

fn generate(cert_path: &Path, key_path: &Path, hosts: &[String]) -> anyhow::Result<()> {
    let mut params = rcgen::CertificateParams::new(hosts.to_vec()).context("构造证书参数失败")?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "lan-share");
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2035, 1, 1);

    let key_pair = rcgen::KeyPair::generate().context("生成私钥失败")?;
    let cert = params.self_signed(&key_pair).context("签发证书失败")?;
    std::fs::write(cert_path, cert.pem())
        .with_context(|| format!("写入证书失败: {}", cert_path.display()))?;
    std::fs::write(key_path, key_pair.serialize_pem())
        .with_context(|| format!("写入私钥失败: {}", key_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}
