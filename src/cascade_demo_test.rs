#[cfg(test)]
mod tests {
    use crate::cascade_demo::add;

    #[test]
    fn test_add() {
        assert_eq!(add(2, 3), 5);
        assert_eq!(add(-1, 1), 0);
        assert_eq!(add(0, 0), 0);
        assert_eq!(add(-5, -3), -8);
    }
}
