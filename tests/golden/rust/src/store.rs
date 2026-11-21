use crate::Circle;

pub struct Store {
    items: Vec<Circle>,
}

impl Store {
    pub fn new() -> Self {
        Store { items: Vec::new() }
    }

    pub fn add(&mut self, c: Circle) {
        self.items.push(c);
        self.log();
    }

    fn log(&self) {
        println!("{} items", self.items.len());
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn seeded() -> Self {
        let mut s = Self::new();
        s.add(Circle::new(1.0));
        s
    }
}
