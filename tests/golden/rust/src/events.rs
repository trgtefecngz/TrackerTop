pub enum Event {
    Created(u32),
    Deleted,
}

pub fn describe(e: &Event) -> &'static str {
    match e {
        Event::Created(_) => "created",
        Event::Deleted => "deleted",
    }
}

pub fn make() -> Event {
    Event::Created(1)
}

pub fn round_trip() -> &'static str {
    describe(&make())
}
