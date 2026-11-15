/// Corrections deliberately avoid repeated tiny seeks, which cause audible stutter.
#[derive(Debug, PartialEq)]
pub enum Correction {
    None,
    Speed(f64),
    Seek(f64),
}

pub fn correction(local: f64, target: f64, paused: bool) -> Correction {
    let error = target - local;
    if !local.is_finite() || !target.is_finite() {
        return Correction::None;
    }
    if error.abs() > 1.5 || (paused && error.abs() > 0.15) {
        Correction::Seek(target.max(0.0))
    } else if !paused && error.abs() > 0.15 {
        Correction::Speed(if error > 0.0 { 1.03 } else { 0.97 })
    } else {
        Correction::Speed(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_smooth_corrections_until_large_drift() {
        assert_eq!(correction(10.0, 10.05, false), Correction::Speed(1.0));
        assert_eq!(correction(10.0, 10.5, false), Correction::Speed(1.03));
        assert_eq!(correction(10.5, 10.0, false), Correction::Speed(0.97));
        assert_eq!(correction(10.0, 12.0, false), Correction::Seek(12.0));
        assert_eq!(correction(10.0, 10.5, true), Correction::Seek(10.5));
        assert_eq!(correction(f64::NAN, 10.0, false), Correction::None);
    }
}
