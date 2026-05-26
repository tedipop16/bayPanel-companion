// Telemetry host: trimite UDP cu CPU temp/usage, MEM usage, GPU usage
// catre placa STM32 care ruleaza wol_web.
//
// Folosire:
//   telemetry-host                            -> trimite la 192.168.1.200:9000 la 1s
//   telemetry-host 192.168.1.200:9000         -> destinatie custom
//   telemetry-host 192.168.1.200:9000 500     -> destinatie + interval ms
//
// GPU se detecteaza automat: NVIDIA (nvidia-smi) -> AMD (sysfs) -> Intel (gt freq).

use std::env;
use std::fs;
use std::net::UdpSocket;
use std::process::Command;
use std::thread;
use std::time::Duration;

const DEFAULT_DEST: &str = "192.168.1.200:9000";
const DEFAULT_INTERVAL_MS: u64 = 1000;
const CMD_LISTEN_PORT: u16 = 9001;

fn read_cpu_temp() -> Option<f32> {
    let mut max: Option<f32> = None;
    let entries = fs::read_dir("/sys/class/thermal").ok()?;
    for entry in entries.flatten() {
        let temp_file = entry.path().join("temp");
        let Ok(s) = fs::read_to_string(&temp_file) else { continue };
        let Ok(milli) = s.trim().parse::<i32>() else { continue };
        let c = milli as f32 / 1000.0;
        if c > 0.0 && c < 150.0 {
            max = Some(max.map_or(c, |m| m.max(c)));
        }
    }
    max
}

fn read_cpu_usage(prev: &mut Option<(u64, u64)>) -> Option<f32> {
    let s = fs::read_to_string("/proc/stat").ok()?;
    let line = s.lines().next()?;
    let nums: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|w| w.parse().ok())
        .collect();
    if nums.len() < 4 {
        return None;
    }
    let idle = nums[3];
    let total: u64 = nums.iter().sum();
    let pct = match *prev {
        Some((p_idle, p_total)) => {
            let d_idle = idle.saturating_sub(p_idle);
            let d_total = total.saturating_sub(p_total);
            if d_total > 0 {
                100.0 * (1.0 - d_idle as f32 / d_total as f32)
            } else {
                0.0
            }
        }
        None => 0.0,
    };
    *prev = Some((idle, total));
    Some(pct)
}

fn read_mem_usage() -> Option<f32> {
    let s = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total: Option<u64> = None;
    let mut avail: Option<u64> = None;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = rest.split_whitespace().next().and_then(|w| w.parse().ok());
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail = rest.split_whitespace().next().and_then(|w| w.parse().ok());
        }
    }
    let t = total?;
    let a = avail?;
    Some(100.0 * (1.0 - a as f32 / t as f32))
}

fn gpu_nvidia() -> Option<f32> {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=utilization.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines().next()?.trim().parse::<f32>().ok()
}

fn gpu_amd() -> Option<f32> {
    let entries = fs::read_dir("/sys/class/drm").ok()?;
    for entry in entries.flatten() {
        let path = entry.path().join("device/gpu_busy_percent");
        if let Ok(s) = fs::read_to_string(&path) {
            if let Ok(v) = s.trim().parse::<f32>() {
                return Some(v);
            }
        }
    }
    None
}

fn gpu_intel() -> Option<f32> {
    let cur = fs::read_to_string("/sys/class/drm/card0/gt_act_freq_mhz").ok()?;
    let max = fs::read_to_string("/sys/class/drm/card0/gt_max_freq_mhz").ok()?;
    let c: f32 = cur.trim().parse().ok()?;
    let m: f32 = max.trim().parse().ok()?;
    if m > 0.0 {
        Some((100.0 * c / m).clamp(0.0, 100.0))
    } else {
        None
    }
}

fn read_gpu_usage() -> Option<f32> {
    gpu_nvidia().or_else(gpu_amd).or_else(gpu_intel)
}

fn command_listener(sock: UdpSocket) {
    let mut buf = [0u8; 1024];
    loop {
        let (n, src) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[cmd] recv error: {}", e);
                continue;
            }
        };
        let cmd = String::from_utf8_lossy(&buf[..n]).trim().to_string();
        println!("[cmd <- {}] {:?} ({} bytes)", src, cmd, n);
        if cmd.is_empty() {
            continue;
        }
        if cmd.eq_ignore_ascii_case("SHUTDOWN") {
            run_shutdown();
        } else {
            // Comanda custom — rulam ca shell -c
            println!("[cmd] exec sh -c {:?}", cmd);
            match Command::new("sh").args(["-c", &cmd]).output() {
                Ok(out) => {
                    println!("[cmd] exit={}", out.status);
                    if !out.stdout.is_empty() {
                        println!("[cmd stdout] {}", String::from_utf8_lossy(&out.stdout));
                    }
                    if !out.stderr.is_empty() {
                        eprintln!("[cmd stderr] {}", String::from_utf8_lossy(&out.stderr));
                    }
                }
                Err(e) => eprintln!("[cmd] sh -c esuat: {}", e),
            }
        }
    }
}

fn run_shutdown() {
    // Incearca pe rand: systemctl poweroff (cu polkit), shutdown -h now (cu sudo NOPASSWD),
    // loginctl poweroff (sesiune utilizator).
    let attempts: &[(&str, &[&str])] = &[
        ("systemctl", &["poweroff"]),
        ("loginctl", &["poweroff"]),
        ("shutdown", &["-h", "now"]),
        ("sudo", &["-n", "shutdown", "-h", "now"]),
    ];
    for (cmd, args) in attempts {
        println!("[shutdown] incerc: {} {}", cmd, args.join(" "));
        match Command::new(cmd).args(*args).output() {
            Ok(out) if out.status.success() => {
                println!("[shutdown] OK cu {}", cmd);
                return;
            }
            Ok(out) => {
                eprintln!(
                    "[shutdown] {} exit={} stderr={}",
                    cmd,
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Err(e) => eprintln!("[shutdown] {} eroare: {}", cmd, e),
        }
    }
    eprintln!("[shutdown] toate metodele au esuat. Verifica permisiunile pentru poweroff.");
}

fn detect_gpu_label() -> &'static str {
    if gpu_nvidia().is_some() {
        "NVIDIA"
    } else if gpu_amd().is_some() {
        "AMD"
    } else if gpu_intel().is_some() {
        "Intel"
    } else {
        "necunoscut (0%)"
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let dest = args.get(1).cloned().unwrap_or_else(|| DEFAULT_DEST.into());
    let interval_ms: u64 = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_INTERVAL_MS);

    let socket = UdpSocket::bind("0.0.0.0:0").expect("bind UDP failed");

    // Detecteaza IP-ul local pe interfata care ajunge la board
    // (connect pe UDP nu trimite nimic, doar alege ruta)
    let local_ip: String = {
        let probe = UdpSocket::bind("0.0.0.0:0").ok();
        probe
            .and_then(|s| s.connect(&dest).ok().map(|_| s))
            .and_then(|s| s.local_addr().ok())
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|| "0.0.0.0".into())
    };

    println!(
        "telemetry-host {} -> {} la fiecare {}ms (GPU: {})",
        local_ip,
        dest,
        interval_ms,
        detect_gpu_label()
    );

    // Asculta comenzi de la placa STM32 (shutdown / custom shell) pe UDP 9001
    let cmd_addr = format!("0.0.0.0:{}", CMD_LISTEN_PORT);
    match UdpSocket::bind(&cmd_addr) {
        Ok(cmd_sock) => {
            println!("Comenzi UDP asculta pe {}", cmd_addr);
            thread::spawn(move || command_listener(cmd_sock));
        }
        Err(e) => eprintln!("Nu pot deschide socket comenzi {}: {}", cmd_addr, e),
    }

    let mut cpu_prev: Option<(u64, u64)> = None;
    // Warm-up: nevoie de doua mostre pentru delta CPU.
    let _ = read_cpu_usage(&mut cpu_prev);
    thread::sleep(Duration::from_millis(200));

    loop {
        let temp = read_cpu_temp().unwrap_or(0.0);
        let cpu = read_cpu_usage(&mut cpu_prev).unwrap_or(0.0);
        let mem = read_mem_usage().unwrap_or(0.0);
        let gpu = read_gpu_usage().unwrap_or(0.0);

        let msg = format!(
            "TEMP={:.1} CPU={:.1} MEM={:.1} GPU={:.1} IP={}\n",
            temp, cpu, mem, gpu, local_ip
        );
        print!("{}", msg);
        if let Err(e) = socket.send_to(msg.as_bytes(), &dest) {
            eprintln!("send error: {}", e);
        }

        thread::sleep(Duration::from_millis(interval_ms));
    }
}
