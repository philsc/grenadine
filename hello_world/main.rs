fn greeting() -> &'static str {
    "Hello, world!"
}

fn main() {
    println!("{}", greeting());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greets_the_world() {
        assert_eq!(greeting(), "Hello, world!");
    }
}
