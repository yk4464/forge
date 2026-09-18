use std::collections::HashMap;
use std::sync::Arc;

/// Generic name-keyed registry. Tools, providers, and (later) skills,
/// hooks and permission policies all plug in through this. The trait-object
/// form `Registry<dyn Trait>` is the intended use.
pub struct Registry<T: ?Sized> {
    items: HashMap<String, Arc<T>>,
}

impl<T: ?Sized> Default for Registry<T> {
    fn default() -> Self {
        Self {
            items: HashMap::new(),
        }
    }
}

impl<T: ?Sized + Named> Registry<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, item: Arc<T>) {
        self.items.insert(item.name().to_string(), item);
    }

    pub fn get(&self, name: &str) -> Option<Arc<T>> {
        self.items.get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.items.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn all(&self) -> Vec<Arc<T>> {
        self.items.values().cloned().collect()
    }
}

/// Types that can live in a registry by name.
pub trait Named {
    fn name(&self) -> &str;
}

// dyn Tool / dyn Skill etc. get their name from the trait object itself.
impl<T: Named + ?Sized> Named for &T {
    fn name(&self) -> &str {
        (**self).name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Dummy(String);
    impl Named for Dummy {
        fn name(&self) -> &str {
            &self.0
        }
    }

    #[test]
    fn register_get_dedupe() {
        let mut r: Registry<Dummy> = Registry::new();
        r.register(Arc::new(Dummy("b".into())));
        r.register(Arc::new(Dummy("a".into())));
        r.register(Arc::new(Dummy("a".into())));
        assert_eq!(r.names(), vec!["a".to_string(), "b".to_string()]);
        assert!(r.get("a").is_some());
        assert!(r.get("c").is_none());
    }
}
