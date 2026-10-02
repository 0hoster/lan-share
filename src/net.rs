use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

/// 探测本机用于访问局域网的 IPv4 地址。
///
/// 采用无依赖方案：UDP `connect` 只是让内核按路由表选择出口网卡，
/// 不会真正发送任何数据包，因此在完全离线的环境下也能正常工作。
pub fn primary_local_ip() -> Option<IpAddr> {
    // 常见的内网/公网目标，用于让内核挑选默认出口网卡。
    const PROBES: [&str; 3] = ["8.8.8.8:80", "1.1.1.1:80", "10.255.255.255:1"];

    for probe in PROBES {
        let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
            continue;
        };
        if sock.connect(probe).is_err() {
            continue;
        }
        if let Ok(SocketAddr::V4(addr)) = sock.local_addr() {
            let ip = *addr.ip();
            if !ip.is_loopback() && !ip.is_unspecified() {
                return Some(IpAddr::V4(ip));
            }
        }
    }

    // 兜底：扫描常见私有网段本机地址（无第三方依赖时的近似做法）。
    for candidate in ["192.168.1.1", "192.168.0.1", "10.0.0.1"] {
        if let Ok(ip) = candidate.parse::<Ipv4Addr>() {
            let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
            if sock.connect((ip, 9)).is_ok() {
                if let Ok(SocketAddr::V4(addr)) = sock.local_addr() {
                    let local = *addr.ip();
                    if !local.is_loopback() && !local.is_unspecified() {
                        return Some(IpAddr::V4(local));
                    }
                }
            }
        }
    }

    None
}
