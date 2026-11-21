use crate::util;

pub trait Shape {
    fn area(&self) -> f64;
}

pub struct Circle {
    r: f64,
}

impl Circle {
    pub fn new(r: f64) -> Self {
        Circle { r }
    }
}

impl Shape for Circle {
    fn area(&self) -> f64 {
        util::mul(self.r, self.r) * 3.14
    }
}

pub struct Square {
    side: f64,
}

impl Square {
    pub fn new(side: f64) -> Self {
        Square { side }
    }
}

impl Shape for Square {
    fn area(&self) -> f64 {
        util::mul(self.side, self.side)
    }
}

pub fn unit_square() -> Square {
    Square::new(1.0)
}

pub fn total_area(shapes: &[Box<dyn Shape>]) -> f64 {
    shapes.iter().map(|s| s.area()).sum()
}
