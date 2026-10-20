use crate::playback::Mode;

#[derive(Debug, Clone, Default)]
pub struct PlaylistCursor {
    pub items: Vec<String>,
    pub mode: Mode,
    pub pos: usize,
    pub order: Vec<usize>,
    pub order_pos: usize,
    pub rng: u64,
    pub current: Option<String>,
}

impl PlaylistCursor {
    pub fn new(items: Vec<String>, mode: Mode, seed: u64) -> Self {
        let mut c = PlaylistCursor {
            items,
            mode,
            rng: if seed == 0 {
                0xdead_beef_cafe_babe
            } else {
                seed
            },
            ..Default::default()
        };
        if matches!(mode, Mode::Shuffle) {
            c.build_order();
        }
        c
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn replace_items(&mut self, items: Vec<String>, mode: Mode) {
        if self.items == items && self.mode == mode {
            return;
        }
        if self.mode != mode {
            let current = self.current.clone();
            *self = Self::new(items, mode, self.rng);
            if let Some(current) = current {
                self.set_current(&current);
            }
            return;
        }
        let mut order = self
            .order
            .iter()
            .filter_map(|index| {
                self.items
                    .get(*index)
                    .and_then(|id| items.iter().position(|item| item == id))
            })
            .collect::<Vec<_>>();
        let mut added = (0..items.len())
            .filter(|index| !order.contains(index))
            .collect::<Vec<_>>();
        if mode == Mode::Shuffle {
            for i in (1..added.len()).rev() {
                let j = self.rng_range(i + 1);
                added.swap(i, j);
            }
        }
        order.extend(added);
        let current = self.current.take();
        self.items = items;
        self.order = order;
        self.pos = 0;
        self.order_pos = 0;
        if let Some(current) = current {
            self.set_current(&current);
        }
    }

    fn rng_next(&mut self) -> u64 {
        let mut x = if self.rng == 0 {
            0xdead_beef_cafe_babe
        } else {
            self.rng
        };
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    fn rng_range(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.rng_next() % n as u64) as usize
    }

    fn build_order(&mut self) {
        let n = self.items.len();
        self.order = (0..n).collect();
        for i in (1..n).rev() {
            let j = self.rng_range(i + 1);
            self.order.swap(i, j);
        }
        self.order_pos = 0;
    }

    pub fn next(&mut self, delta: i32) -> Option<String> {
        let n = self.items.len();
        if n == 0 {
            return None;
        }
        let idx = match self.mode {
            Mode::Sequential => {
                let step = delta.rem_euclid(n as i32) as usize;
                self.pos = (self.pos + step) % n;
                self.pos
            }
            Mode::Random => {
                let mut idx = self.rng_range(n);
                if n > 1 && Some(&self.items[idx]) == self.current.as_ref() {
                    idx = (idx + 1) % n;
                }
                self.pos = idx;
                idx
            }
            Mode::Shuffle => {
                if self.order.len() != n {
                    self.build_order();
                }
                let m = self.order.len();
                let step = delta.rem_euclid(m as i32) as usize;
                self.order_pos = (self.order_pos + step) % m;
                if self.order_pos == 0 {
                    self.build_order();
                }
                self.order[self.order_pos]
            }
        };
        let id = self.items[idx].clone();
        self.current = Some(id.clone());
        Some(id)
    }

    pub fn set_current(&mut self, id: &str) -> bool {
        let Some(idx) = self.items.iter().position(|x| x == id) else {
            return false;
        };
        self.pos = idx;
        if matches!(self.mode, Mode::Shuffle) {
            if self.order.len() != self.items.len() {
                self.build_order();
            }
            if let Some(op) = self.order.iter().position(|&o| o == idx) {
                self.order_pos = op;
            }
        }
        self.current = Some(id.to_string());
        true
    }

    pub fn first(&mut self) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        let id = match self.mode {
            Mode::Shuffle => {
                if self.order.len() != self.items.len() {
                    self.build_order();
                }
                self.order_pos = 0;
                self.items[self.order[0]].clone()
            }
            Mode::Random => {
                let idx = self.rng_range(self.items.len());
                self.pos = idx;
                self.items[idx].clone()
            }
            _ => {
                self.pos = 0;
                self.items[0].clone()
            }
        };
        self.current = Some(id.clone());
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<String> {
        vec!["a".into(), "b".into(), "c".into()]
    }

    #[test]
    fn sequential_wraps_forward() {
        let mut c = PlaylistCursor::new(items(), Mode::Sequential, 1);
        assert_eq!(c.first().as_deref(), Some("a"));
        assert_eq!(c.next(1).as_deref(), Some("b"));
        assert_eq!(c.next(1).as_deref(), Some("c"));
        assert_eq!(c.next(1).as_deref(), Some("a"));
    }

    #[test]
    fn sequential_steps_backward() {
        let mut c = PlaylistCursor::new(items(), Mode::Sequential, 1);
        c.first();
        assert_eq!(c.next(-1).as_deref(), Some("c"));
    }

    #[test]
    fn empty_yields_none() {
        let mut c = PlaylistCursor::new(vec![], Mode::Sequential, 1);
        assert_eq!(c.next(1), None);
        assert_eq!(c.first(), None);
    }

    #[test]
    fn shuffle_covers_all_before_repeat() {
        let mut c = PlaylistCursor::new(items(), Mode::Shuffle, 42);
        let mut seen = std::collections::HashSet::new();
        seen.insert(c.first().unwrap());
        seen.insert(c.next(1).unwrap());
        seen.insert(c.next(1).unwrap());
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn shuffle_deterministic_with_seed() {
        let mut a = PlaylistCursor::new(items(), Mode::Shuffle, 7);
        let mut b = PlaylistCursor::new(items(), Mode::Shuffle, 7);
        for _ in 0..6 {
            assert_eq!(a.next(1), b.next(1));
        }
    }

    #[test]
    fn random_avoids_immediate_repeat() {
        let mut c = PlaylistCursor::new(items(), Mode::Random, 3);
        let mut prev = c.first().unwrap();
        for _ in 0..20 {
            let cur = c.next(1).unwrap();
            assert_ne!(cur, prev);
            prev = cur;
        }
    }

    #[test]
    fn member_refresh_preserves_current_and_shuffle_history() {
        let mut cursor = PlaylistCursor::new(items(), Mode::Shuffle, 42);
        cursor.first();
        cursor.next(1);
        let current = cursor.current.clone();
        let old_order = cursor
            .order
            .iter()
            .map(|i| cursor.items[*i].clone())
            .collect::<Vec<_>>();
        cursor.replace_items(
            vec!["c".into(), "b".into(), "a".into(), "d".into()],
            Mode::Shuffle,
        );
        assert_eq!(cursor.current, current);
        assert_eq!(
            cursor
                .order
                .iter()
                .take(3)
                .map(|i| cursor.items[*i].clone())
                .collect::<Vec<_>>(),
            old_order
        );
        cursor.replace_items(vec![], Mode::Shuffle);
        assert_eq!(cursor.current, None);
        assert_eq!(cursor.first(), None);
    }
}
