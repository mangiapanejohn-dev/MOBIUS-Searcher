//! Graph Workspace registry: ordered, stable ids (never reused), one active
//! graph, no duplicate metrics, bounded count.

use searcher_core::metrics::MetricId;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChartStyle {
    Line,
    Candle,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LineStyle {
    /// ╭──╯ box-drawing steps (default, crisp)
    Box,
    /// 2×4 braille sub-cells (higher resolution)
    Braille,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphEntry {
    pub id: u32,
    pub metric: MetricId,
    pub style: ChartStyle,
}

#[derive(Clone, Debug)]
pub struct Workspace {
    pub entries: Vec<GraphEntry>,
    pub active: usize,
    pub max: usize,
    next_id: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AddError {
    Duplicate,
    Limit(usize),
}

impl Workspace {
    pub fn new(max: usize, defaults: &[MetricId]) -> Self {
        let mut w = Self { entries: Vec::new(), active: 0, max: max.clamp(1, 16), next_id: 1 };
        for m in defaults {
            let _ = w.add(*m);
        }
        w.active = 0;
        w
    }

    pub fn add(&mut self, m: MetricId) -> Result<u32, AddError> {
        if self.entries.iter().any(|e| e.metric == m) {
            return Err(AddError::Duplicate);
        }
        if self.entries.len() >= self.max {
            return Err(AddError::Limit(self.max));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(GraphEntry { id, metric: m, style: ChartStyle::Line });
        self.active = self.entries.len() - 1;
        Ok(id)
    }

    pub fn remove_active(&mut self) -> Option<GraphEntry> {
        if self.entries.is_empty() {
            return None;
        }
        let e = self.entries.remove(self.active);
        self.active = self.active.min(self.entries.len().saturating_sub(1));
        Some(e)
    }

    pub fn contains(&self, m: MetricId) -> bool {
        self.entries.iter().any(|e| e.metric == m)
    }

    pub fn select(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        let n = self.entries.len() as i32;
        self.active = (self.active as i32 + delta).rem_euclid(n) as usize;
    }

    pub fn move_active(&mut self, delta: i32) {
        let n = self.entries.len() as i32;
        let to = self.active as i32 + delta;
        if n < 2 || to < 0 || to >= n {
            return;
        }
        self.entries.swap(self.active, to as usize);
        self.active = to as usize;
    }

    pub fn active(&self) -> Option<&GraphEntry> {
        self.entries.get(self.active)
    }

    pub fn active_metric(&self) -> Option<MetricId> {
        self.active().map(|e| e.metric)
    }

    pub fn toggle_style(&mut self) {
        if let Some(e) = self.entries.get_mut(self.active) {
            e.style = match e.style {
                ChartStyle::Line => ChartStyle::Candle,
                ChartStyle::Candle => ChartStyle::Line,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_invariants() {
        let mut w = Workspace::new(3, &[MetricId::Price, MetricId::NetEdge]);
        assert_eq!(w.active, 0);
        assert_eq!(w.add(MetricId::Price), Err(AddError::Duplicate));
        let id3 = w.add(MetricId::Pnl).unwrap();
        assert_eq!(w.add(MetricId::Equity), Err(AddError::Limit(3)));
        assert_eq!(w.active_metric(), Some(MetricId::Pnl));
        w.remove_active();
        let id4 = w.add(MetricId::Equity).unwrap();
        assert!(id4 > id3, "ids are never reused");
        w.active = 0;
        w.move_active(1);
        assert_eq!(w.entries[1].metric, MetricId::Price);
        assert_eq!(w.active, 1);
        w.select(5);
        assert!(w.active < w.entries.len());
        w.toggle_style();
        assert_eq!(w.active().unwrap().style, ChartStyle::Candle);
        while w.remove_active().is_some() {}
        assert!(w.active().is_none());
    }
}
