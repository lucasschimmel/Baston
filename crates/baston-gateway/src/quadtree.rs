//! Lightweight region quadtree over the GTA V map plane (−4000..+4000 on X/Y).
//!
//! Stores zone bounding boxes; supports O(log n) point lookup and AABB
//! intersection queries. Custom implementation — zone counts are small, so a
//! simple node-fitting tree beats pulling in an external crate.

use baston_protocol::Aabb;

const MAX_DEPTH: usize = 8;
const NODE_CAPACITY: usize = 4;

#[derive(Debug)]
struct Node {
    bounds: Aabb,
    /// Items that don't fit entirely into a single child.
    items: Vec<(String, Aabb)>,
    children: Option<Box<[Node; 4]>>,
    depth: usize,
}

impl Node {
    fn new(bounds: Aabb, depth: usize) -> Self {
        Self {
            bounds,
            items: Vec::new(),
            children: None,
            depth,
        }
    }

    fn quadrants(&self) -> [Aabb; 4] {
        let (cx, cy) = self.bounds.center();
        [
            Aabb::new(self.bounds.x_min, self.bounds.y_min, cx, cy),
            Aabb::new(cx, self.bounds.y_min, self.bounds.x_max, cy),
            Aabb::new(self.bounds.x_min, cy, cx, self.bounds.y_max),
            Aabb::new(cx, cy, self.bounds.x_max, self.bounds.y_max),
        ]
    }

    fn insert(&mut self, id: String, bounds: Aabb) {
        if self.children.is_none() && self.items.len() >= NODE_CAPACITY && self.depth < MAX_DEPTH {
            let quads = self.quadrants();
            self.children = Some(Box::new(quads.map(|q| Node::new(q, self.depth + 1))));
            // Re-sink existing items that now fit a child.
            let items = std::mem::take(&mut self.items);
            for (iid, ib) in items {
                self.insert(iid, ib);
            }
        }
        if let Some(children) = self.children.as_mut() {
            if let Some(idx) = {
                let this = &*children;
                let quads: [Aabb; 4] = [
                    this[0].bounds,
                    this[1].bounds,
                    this[2].bounds,
                    this[3].bounds,
                ];
                quads.iter().position(|q| {
                    bounds.x_min >= q.x_min
                        && bounds.x_max <= q.x_max
                        && bounds.y_min >= q.y_min
                        && bounds.y_max <= q.y_max
                })
            } {
                children[idx].insert(id, bounds);
                return;
            }
        }
        self.items.push((id, bounds));
    }

    fn remove(&mut self, id: &str) -> bool {
        if let Some(pos) = self.items.iter().position(|(iid, _)| iid == id) {
            self.items.remove(pos);
            return true;
        }
        if let Some(children) = self.children.as_mut() {
            return children.iter_mut().any(|c| c.remove(id));
        }
        false
    }

    fn query_point<'a>(&'a self, x: f32, y: f32, out: &mut Vec<&'a str>) {
        for (id, b) in &self.items {
            if b.contains(x, y) {
                out.push(id);
            }
        }
        if let Some(children) = self.children.as_ref() {
            for c in children.iter() {
                if c.bounds.contains(x, y) {
                    c.query_point(x, y, out);
                }
            }
        }
    }

    fn query_aabb<'a>(&'a self, area: &Aabb, out: &mut Vec<&'a str>) {
        for (id, b) in &self.items {
            if b.intersects(area) {
                out.push(id);
            }
        }
        if let Some(children) = self.children.as_ref() {
            for c in children.iter() {
                if c.bounds.intersects(area) {
                    c.query_aabb(area, out);
                }
            }
        }
    }
}

/// Spatial index mapping map coordinates to zone ids.
#[derive(Debug)]
pub struct QuadTree {
    root: Node,
}

impl QuadTree {
    /// GTA V world plane. Zones registering outside are still stored (at root).
    pub fn gta_v() -> Self {
        Self::new(Aabb::new(-4000.0, -4000.0, 4000.0, 4000.0))
    }

    pub fn new(bounds: Aabb) -> Self {
        Self {
            root: Node::new(bounds, 0),
        }
    }

    pub fn insert(&mut self, zone_id: &str, bounds: Aabb) {
        self.remove(zone_id); // idempotent re-registration
        self.root.insert(zone_id.to_owned(), bounds);
    }

    pub fn remove(&mut self, zone_id: &str) -> bool {
        self.root.remove(zone_id)
    }

    /// First zone whose bounds contain the point (zones shouldn't overlap).
    pub fn query_point(&self, x: f32, y: f32) -> Option<String> {
        let mut out = Vec::new();
        self.root.query_point(x, y, &mut out);
        out.first().map(|s| s.to_string())
    }

    /// All zones intersecting the rectangle (cross-zone AoI, jalon D5).
    pub fn query_aabb(&self, area: &Aabb) -> Vec<String> {
        let mut out = Vec::new();
        self.root.query_aabb(area, &mut out);
        out.into_iter().map(str::to_owned).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_zone_tree() -> QuadTree {
        let mut qt = QuadTree::gta_v();
        qt.insert("zone-a", Aabb::new(-4000.0, -4000.0, 0.0, 4000.0));
        qt.insert("zone-b", Aabb::new(0.0, -4000.0, 4000.0, 4000.0));
        qt
    }

    #[test]
    fn point_lookup_routes_to_correct_zone() {
        let qt = two_zone_tree();
        assert_eq!(qt.query_point(-500.0, 200.0).as_deref(), Some("zone-a"));
        assert_eq!(qt.query_point(1500.0, -300.0).as_deref(), Some("zone-b"));
        assert_eq!(qt.query_point(5000.0, 5000.0), None);
    }

    #[test]
    fn remove_then_miss() {
        let mut qt = two_zone_tree();
        assert!(qt.remove("zone-a"));
        assert_eq!(qt.query_point(-500.0, 200.0), None);
        assert!(!qt.remove("zone-a"));
    }

    #[test]
    fn aabb_query_finds_intersecting_zones() {
        let qt = two_zone_tree();
        let mut hits = qt.query_aabb(&Aabb::new(-100.0, -100.0, 100.0, 100.0));
        hits.sort();
        assert_eq!(hits, vec!["zone-a", "zone-b"]);
    }

    #[test]
    fn subdivision_keeps_items_reachable() {
        let mut qt = QuadTree::gta_v();
        for i in 0..32 {
            let x = -4000.0 + (i as f32) * 250.0;
            qt.insert(&format!("z{i}"), Aabb::new(x, -4000.0, x + 250.0, 4000.0));
        }
        assert_eq!(qt.query_point(-3990.0, 0.0).as_deref(), Some("z0"));
        assert_eq!(qt.query_point(3990.0, 0.0).as_deref(), Some("z31"));
    }
}
