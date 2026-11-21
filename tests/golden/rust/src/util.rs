pub fn mul(a: f64, b: f64) -> f64 {
    a * b
}

pub fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    x.max(lo).min(hi)
}
