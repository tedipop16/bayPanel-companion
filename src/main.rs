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
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::Command;
use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use std::thread;
use std::time::Duration;

const DEFAULT_DEST: &str = "192.168.1.200:9000";
const DEFAULT_INTERVAL_MS: u64 = 1000;
const CMD_LISTEN_PORT: u16 = 9001;
const WEATHER_REFRESH_SECS: u64 = 5; // 10 minute
const SENTINEL_UNSET: i32 = i32::MIN;

// Weather code mapping (0=unknown, 1=sun, 2=cloud, 3=rain, 4=snow)
static WEATHER_KIND: AtomicU8 = AtomicU8::new(0);
// Temperatura exterior in zecimi de °C (ex: 2513 = 25.3°C)
static OUTDOOR_TEMP_TENTHS: AtomicI32 = AtomicI32::new(SENTINEL_UNSET);

fn read_cpu_temp() -> Option<f32> {
    let mut max: Option<f32> = None;
    let entries = fs::read_dir("/sys/class/thermal").ok()?;
    for entry in entries.flatten() {
        let temp_file = entry.path().join("temp");
        let Ok(s) = fs::read_to_string(&temp_file) else {
            continue;
        };
        let Ok(milli) = s.trim().parse::<i32>() else {
            continue;
        };
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

fn command_listener_tcp(listener: TcpListener) {
    for incoming in listener.incoming() {
        let mut stream = match incoming {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[cmd] accept err: {}", e);
                continue;
            }
        };
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "?".into());
        stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
        let mut buf = Vec::with_capacity(256);
        if let Err(e) = stream.read_to_end(&mut buf) {
            eprintln!("[cmd <- {}] read err: {}", peer, e);
            continue;
        }
        let cmd = String::from_utf8_lossy(&buf).trim().to_string();
        println!("[cmd <- {}] {:?} ({} bytes)", peer, cmd, buf.len());
        if cmd.is_empty() {
            continue;
        }
        if cmd.eq_ignore_ascii_case("SHUTDOWN") {
            run_shutdown();
        } else {
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

// ── Geolocation + Weather ────────────────────────────────────────────────────

fn fetch_url(url: &str) -> Option<String> {
    let out = match Command::new("curl")
        .args([
            "-sS",
            "-4", // forteaza IPv4 (evita timeout-uri IPv6)
            "-L",
            "--connect-timeout",
            "5",
            "--max-time",
            "15",
            url,
        ])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[fetch] curl spawn err pe {}: {}", url, e);
            return None;
        }
    };
    if !out.status.success() {
        eprintln!(
            "[fetch] {} exit={} stderr={:?}",
            url,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    match String::from_utf8(out.stdout) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("[fetch] utf8 err pe {}: {}", url, e);
            None
        }
    }
}

fn extract_number(json: &str, key: &str) -> Option<f64> {
    // open-meteo are "temperature_2m":"°C" in current_units si "temperature_2m":20.3 in current.
    // Itereaza prin toate aparitiile cheii pana gasim una cu valoare numerica.
    let pattern = format!("\"{}\"", key);
    let mut cursor = 0;
    while let Some(rel) = json[cursor..].find(&pattern) {
        let key_end = cursor + rel + pattern.len();
        let rest = &json[key_end..];
        let Some(colon) = rest.find(':') else {
            cursor = key_end;
            continue;
        };
        let after = rest[colon + 1..].trim_start();
        if after.starts_with('"') {
            // Valoare string, sarim
            cursor = key_end;
            continue;
        }
        let end = after
            .find(|c: char| c == ',' || c == '}' || c == ']')
            .unwrap_or(after.len());
        if let Ok(v) = after[..end].trim().parse::<f64>() {
            return Some(v);
        }
        cursor = key_end;
    }
    None
}

fn extract_string<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    let quote = after.strip_prefix('"')?;
    let end = quote.find('"')?;
    Some(&quote[..end])
}

fn fetch_location() -> Option<(f64, f64, String)> {
    let json = fetch_url("http://ip-api.com/json/?fields=lat,lon,city,status")?;
    if extract_string(&json, "status") != Some("success") {
        eprintln!("[weather] ip-api status != success: {}", json);
        return None;
    }
    let lat = extract_number(&json, "lat")?;
    let lon = extract_number(&json, "lon")?;
    let city = extract_string(&json, "city").unwrap_or("?").to_string();
    Some((lat, lon, city))
}

fn fetch_weather(lat: f64, lon: f64) -> Option<(f64, u32)> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={:.4}&longitude={:.4}&current=temperature_2m,weather_code",
        lat, lon
    );
    let json = fetch_url(&url)?;
    let temp = match extract_number(&json, "temperature_2m") {
        Some(v) => v,
        None => {
            eprintln!(
                "[weather] temperature_2m lipseste sau invalid. Response: {}",
                json
            );
            return None;
        }
    };
    let code = match extract_number(&json, "weather_code") {
        Some(v) => v as u32,
        None => {
            eprintln!(
                "[weather] weather_code lipseste sau invalid. Response: {}",
                json
            );
            return None;
        }
    };
    Some((temp, code))
}

fn wmo_code_to_kind(code: u32) -> u8 {
    // WMO codes -> 1=sun, 2=cloud, 3=rain, 4=snow
    match code {
        0 | 1 => 1,   // clear / mainly clear
        2 | 3 => 2,   // partly cloudy / overcast
        45 | 48 => 2, // fog
        51..=67 => 3, // drizzle / rain
        71..=77 => 4, // snow fall
        80..=82 => 3, // rain showers
        85 | 86 => 4, // snow showers
        95..=99 => 3, // thunder (tratam ca ploaie)
        _ => 0,       // unknown
    }
}

fn kind_str(k: u8) -> &'static str {
    match k {
        1 => "sun",
        2 => "cloud",
        3 => "rain",
        4 => "snow",
        _ => "unknown",
    }
}

fn weather_loop() {
    loop {
        match fetch_location() {
            Some((lat, lon, city)) => {
                println!("[weather] locatie: {} ({:.3}, {:.3})", city, lat, lon);
                match fetch_weather(lat, lon) {
                    Some((temp, code)) => {
                        let kind = wmo_code_to_kind(code);
                        OUTDOOR_TEMP_TENTHS.store((temp * 10.0).round() as i32, Ordering::Relaxed);
                        WEATHER_KIND.store(kind, Ordering::Relaxed);
                        println!(
                            "[weather] temp={:.1}°C code={} -> {}",
                            temp,
                            code,
                            kind_str(kind)
                        );
                    }
                    None => eprintln!("[weather] open-meteo a esuat"),
                }
            }
            None => eprintln!("[weather] geolocatia a esuat (ip-api.com)"),
        }
        thread::sleep(Duration::from_secs(WEATHER_REFRESH_SECS));
    }
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
        "telemetry-host {} -> {} TCP la fiecare {}ms (GPU: {})",
        local_ip,
        dest,
        interval_ms,
        detect_gpu_label()
    );

    // TCP listener pentru comenzi (shutdown / custom shell) pe port 9001
    let cmd_addr = format!("0.0.0.0:{}", CMD_LISTEN_PORT);
    match TcpListener::bind(&cmd_addr) {
        Ok(listener) => {
            println!("Comenzi TCP asculta pe {}", cmd_addr);
            thread::spawn(move || command_listener_tcp(listener));
        }
        Err(e) => eprintln!("Nu pot deschide TCP listener {}: {}", cmd_addr, e),
    }

    // Thread separat care actualizeaza vremea
    thread::spawn(weather_loop);

    let mut cpu_prev: Option<(u64, u64)> = None;
    // Warm-up: nevoie de doua mostre pentru delta CPU.
    let _ = read_cpu_usage(&mut cpu_prev);
    thread::sleep(Duration::from_millis(200));

    // Loop principal: deschide TCP, trimite line-by-line, reconnect la eroare
    loop {
        let mut stream = match TcpStream::connect(&dest) {
            Ok(s) => {
                println!("[tcp] conectat la {}", dest);
                s
            }
            Err(e) => {
                eprintln!("[tcp] connect {} esuat: {}. Retry 3s", dest, e);
                thread::sleep(Duration::from_secs(3));
                continue;
            }
        };
        stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
        // Dezactivez Nagle ca trimiterea de 1Hz sa nu fie amanata.
        let _ = stream.set_nodelay(true);

        loop {
            let temp = read_cpu_temp().unwrap_or(0.0);
            let cpu = read_cpu_usage(&mut cpu_prev).unwrap_or(0.0);
            let mem = read_mem_usage().unwrap_or(0.0);
            let gpu = read_gpu_usage().unwrap_or(0.0);

            let wkind = WEATHER_KIND.load(Ordering::Relaxed);
            let out_tenths = OUTDOOR_TEMP_TENTHS.load(Ordering::Relaxed);
            let msg = if wkind != 0 && out_tenths != SENTINEL_UNSET {
                let out_c = (out_tenths as f32) / 10.0;
                format!(
                    "TEMP={:.1} CPU={:.1} MEM={:.1} GPU={:.1} IP={} WEATHER={} OUT={:.1}\n",
                    temp,
                    cpu,
                    mem,
                    gpu,
                    local_ip,
                    kind_str(wkind),
                    out_c
                )
            } else {
                format!(
                    "TEMP={:.1} CPU={:.1} MEM={:.1} GPU={:.1} IP={}\n",
                    temp, cpu, mem, gpu, local_ip
                )
            };
            print!("{}", msg);
            if let Err(e) = stream.write_all(msg.as_bytes()) {
                eprintln!("[tcp] write esuat: {}. Reconnect.", e);
                break;
            }
            thread::sleep(Duration::from_millis(interval_ms));
        }
    }
}
