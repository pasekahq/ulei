//! `ulei-connector-test`, a C11 stdio connector over a synthetic source. It reads one `Request`
//! line on stdin and writes `limit` items numbered after its cursor, each followed by a
//! checkpoint, then `done`. With a key file, each item's text carries the file's SHA-256.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, BufWriter, Write};
use std::process::ExitCode;

use ulei_contracts::generated::c11::{ErrorCode, Line, LineType, Request};

/// What one run emits: items `after + 1` to `after + limit`.
struct Run {
    after: u64,
    limit: u64,
    key_sha256: Option<String>,
}

fn main() -> ExitCode {
    let mut out = BufWriter::new(io::stdout().lock());
    let written = match start() {
        Ok(run) => emit(&mut out, &run).map(|()| ExitCode::SUCCESS),
        Err((code, message)) => {
            write_line(&mut out, &error(code, message)).map(|()| ExitCode::FAILURE)
        }
    };
    match written.and_then(|code| out.flush().map(|()| code)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ulei-connector-test: stdout: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The run the request on stdin asks for, or the code and message of the `error` that ends it.
fn start() -> Result<Run, (ErrorCode, String)> {
    let mut text = String::new();
    io::stdin()
        .lock()
        .read_line(&mut text)
        .map_err(|e| (ErrorCode::Internal, format!("stdin: {e}")))?;
    let request: Request = serde_json::from_str(text.trim_end())
        .map_err(|e| (ErrorCode::BadRequest, format!("request: {e}")))?;
    let after = match request.cursor.as_deref() {
        None => 0,
        Some(cursor) => cursor.parse().map_err(|_| {
            (
                ErrorCode::BadCursor,
                "cursor is not an item number".to_owned(),
            )
        })?,
    };
    let key_sha256 = match request.key_file {
        None => None,
        Some(path) => {
            let bytes =
                fs::read(&path).map_err(|e| (ErrorCode::KeyUnreadable, format!("{path}: {e}")))?;
            Some(sha256_hex(&bytes))
        }
    };
    Ok(Run {
        after,
        limit: request.limit,
        key_sha256,
    })
}

fn emit(out: &mut impl Write, run: &Run) -> io::Result<()> {
    let last = run.after.saturating_add(run.limit);
    for n in (run.after..last).map(|n| n + 1) {
        let mut text = format!("synthetic item {n}");
        if let Some(digest) = &run.key_sha256 {
            text.push_str("\nkey sha256:");
            text.push_str(digest);
        }
        let mut item = line(LineType::Item);
        item.external_id = Some(format!("item-{n}"));
        item.kind = Some("text/plain".to_owned());
        item.bytes_b64 = Some(base64(text.as_bytes()));
        item.text = Some(text);
        write_line(out, &item)?;
        let mut checkpoint = line(LineType::Checkpoint);
        checkpoint.cursor = Some(n.to_string());
        write_line(out, &checkpoint)?;
        out.flush()?;
    }
    let mut done = line(LineType::Done);
    done.cursor = (last > run.after).then(|| last.to_string());
    write_line(out, &done)
}

fn write_line(out: &mut impl Write, line: &Line) -> io::Result<()> {
    serde_json::to_writer(&mut *out, line)?;
    out.write_all(b"\n")
}

fn line(r#type: LineType) -> Line {
    Line {
        r#type,
        external_id: None,
        who: None,
        at: None,
        kind: None,
        text: None,
        bytes_b64: None,
        cursor: None,
        code: None,
        message: None,
        retry_after: None,
    }
}

fn error(code: ErrorCode, message: String) -> Line {
    let mut line = line(LineType::Error);
    line.code = Some(code);
    line.message = Some(message);
    line
}

/// Standard base64 with padding (RFC 4648 section 4).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(group >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// SHA-256 (FIPS 180-4) of `data` as lowercase hex. The crate's dependencies are fixed by
/// ADR-0056 to the contract types, so the digest is computed here.
fn sha256_hex(data: &[u8]) -> String {
    let mut state: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let bits = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_be_bytes());
    for block in message.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (word, bytes) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = state;
        for (k, wi) in SHA256_K.iter().zip(w) {
            let e = v[4];
            let a = v[0];
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & v[5]) ^ (!e & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(*k)
                .wrapping_add(wi);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & v[1]) ^ (a & v[2]) ^ (v[1] & v[2]);
            v.rotate_right(1);
            v[4] = v[4].wrapping_add(t1);
            v[0] = t1.wrapping_add(s0.wrapping_add(maj));
        }
        for (s, x) in state.iter_mut().zip(v) {
            *s = s.wrapping_add(x);
        }
    }
    state.iter().fold(String::with_capacity(64), |mut hex, x| {
        let _ = write!(hex, "{x:08x}");
        hex
    })
}
