mod events;
mod shapes;
mod store;
mod util;

pub use shapes::Circle;

/// Squares a value through the util module.
macro_rules! square {
    ($x:expr) => {
        util::mul($x, $x)
    };
}

pub fn squared_area(r: f64) -> f64 {
    square!(r) * 3.0
}

pub fn report(r: f64) {
    println!("{}", util::clamp(r, 0.0, 1.0));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shapes::Shape;

    #[test]
    fn circle_has_area() {
        assert!(Circle::new(1.0).area() > 3.0);
    }

    #[test]
    fn store_counts() {
        let mut s = store::Store::new();
        s.add(Circle::new(2.0));
        assert_eq!(s.len(), 1);
    }
}
