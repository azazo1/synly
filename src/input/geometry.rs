use anyhow::{Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ScreenEdge {
    Left,
    Right,
    Top,
    Bottom,
}

impl ScreenEdge {
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }

    pub fn as_arg(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Top => "top",
            Self::Bottom => "bottom",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl DisplayRect {
    pub fn right(self) -> i32 {
        self.x.saturating_add(self.width)
    }

    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.height)
    }

    pub fn contains(self, point: Point) -> bool {
        point.x >= self.x
            && point.x < self.right()
            && point.y >= self.y
            && point.y < self.bottom()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DesktopLayout {
    pub displays: Vec<DisplayRect>,
}

impl DesktopLayout {
    pub fn new(displays: Vec<DisplayRect>) -> Result<Self> {
        if displays.is_empty() {
            bail!("没有检测到可用显示器");
        }
        if displays
            .iter()
            .any(|display| display.width <= 0 || display.height <= 0)
        {
            bail!("显示器布局包含无效尺寸");
        }
        Ok(Self { displays })
    }

    pub fn is_jump_zone_point(&self, edge: ScreenEdge, point: Point, zone_size: i32) -> bool {
        if zone_size <= 0 {
            return false;
        }
        self.outer_edge_segments(edge).into_iter().any(|segment| {
            let on_span = match edge {
                ScreenEdge::Left | ScreenEdge::Right => {
                    point.y >= segment.start && point.y < segment.end
                }
                ScreenEdge::Top | ScreenEdge::Bottom => {
                    point.x >= segment.start && point.x < segment.end
                }
            };
            on_span
                && match edge {
                    ScreenEdge::Left => {
                        point.x < segment.boundary.saturating_add(zone_size)
                    }
                    ScreenEdge::Right => {
                        point.x >= segment.boundary.saturating_sub(zone_size)
                    }
                    ScreenEdge::Top => {
                        point.y < segment.boundary.saturating_add(zone_size)
                    }
                    ScreenEdge::Bottom => {
                        point.y >= segment.boundary.saturating_sub(zone_size)
                    }
                }
        })
    }

    pub fn normalized_edge_position(&self, edge: ScreenEdge, point: Point) -> f32 {
        let segments = self.outer_edge_segments(edge);
        let coordinate = match edge {
            ScreenEdge::Left | ScreenEdge::Right => point.y,
            ScreenEdge::Top | ScreenEdge::Bottom => point.x,
        };
        let total = segments
            .iter()
            .map(|segment| segment.end - segment.start)
            .sum::<i32>()
            .max(1);
        let mut offset = 0i32;
        for segment in &segments {
            if coordinate >= segment.start
                && coordinate < segment.end
                && point_matches_boundary(edge, point, segment.boundary)
            {
                return ((offset + coordinate - segment.start) as f32 / total as f32)
                    .clamp(0.0, 1.0);
            }
            offset += segment.end - segment.start;
        }
        0.0
    }

    pub fn point_inside_edge(&self, edge: ScreenEdge, position: f32, inset: i32) -> Point {
        let segments = self.outer_edge_segments(edge);
        let total = segments
            .iter()
            .map(|segment| segment.end - segment.start)
            .sum::<i32>()
            .max(1);
        let mut target = (position.clamp(0.0, 1.0) * total as f32) as i32;
        let selected = segments.last().copied().unwrap_or(EdgeSegment {
            start: 0,
            end: 1,
            boundary: 0,
        });
        let mut selected = selected;
        for segment in segments {
            let length = segment.end - segment.start;
            if target < length {
                selected = segment;
                break;
            }
            target -= length;
        }
        let coordinate = selected.start + target.min((selected.end - selected.start - 1).max(0));
        match edge {
            ScreenEdge::Left => Point {
                x: selected.boundary + inset,
                y: coordinate,
            },
            ScreenEdge::Right => Point {
                x: selected.boundary - inset - 1,
                y: coordinate,
            },
            ScreenEdge::Top => Point {
                x: coordinate,
                y: selected.boundary + inset,
            },
            ScreenEdge::Bottom => Point {
                x: coordinate,
                y: selected.boundary - inset - 1,
            },
        }
    }

    pub fn crossed_outer_edge_position(
        &self,
        edge: ScreenEdge,
        point: Point,
        dx: i32,
        dy: i32,
    ) -> Option<f32> {
        let target = Point {
            x: point.x.saturating_add(dx),
            y: point.y.saturating_add(dy),
        };
        for segment in self.outer_edge_segments(edge) {
            let boundary_point = match edge {
                ScreenEdge::Left
                    if dx < 0 && point.x >= segment.boundary && target.x < segment.boundary =>
                {
                    Point {
                        x: segment.boundary,
                        y: crossing_coordinate(point.y, dy, point.x, dx, segment.boundary),
                    }
                }
                ScreenEdge::Right
                    if dx > 0 && point.x < segment.boundary && target.x >= segment.boundary =>
                {
                    Point {
                        x: segment.boundary - 1,
                        y: crossing_coordinate(point.y, dy, point.x, dx, segment.boundary),
                    }
                }
                ScreenEdge::Top
                    if dy < 0 && point.y >= segment.boundary && target.y < segment.boundary =>
                {
                    Point {
                        x: crossing_coordinate(point.x, dx, point.y, dy, segment.boundary),
                        y: segment.boundary,
                    }
                }
                ScreenEdge::Bottom
                    if dy > 0 && point.y < segment.boundary && target.y >= segment.boundary =>
                {
                    Point {
                        x: crossing_coordinate(point.x, dx, point.y, dy, segment.boundary),
                        y: segment.boundary - 1,
                    }
                }
                _ => continue,
            };
            let coordinate = match edge {
                ScreenEdge::Left | ScreenEdge::Right => boundary_point.y,
                ScreenEdge::Top | ScreenEdge::Bottom => boundary_point.x,
            };
            if coordinate >= segment.start && coordinate < segment.end {
                return Some(self.normalized_edge_position(edge, boundary_point));
            }
        }
        None
    }

    pub fn move_within_layout(&self, point: Point, dx: i32, dy: i32) -> Point {
        let target = Point {
            x: point.x.saturating_add(dx),
            y: point.y.saturating_add(dy),
        };
        if self.displays.iter().any(|display| display.contains(target)) {
            return target;
        }
        self.displays
            .iter()
            .map(|display| Point {
                x: target.x.clamp(display.x, display.right() - 1),
                y: target.y.clamp(display.y, display.bottom() - 1),
            })
            .min_by_key(|candidate| {
                let dx = i64::from(candidate.x) - i64::from(target.x);
                let dy = i64::from(candidate.y) - i64::from(target.y);
                dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy))
            })
            .unwrap_or(point)
    }

    fn outer_edge_segments(&self, edge: ScreenEdge) -> Vec<EdgeSegment> {
        let mut segments = Vec::new();
        for display in &self.displays {
            let (start, end, boundary, outside) = match edge {
                ScreenEdge::Left => (display.y, display.bottom(), display.x, Point { x: display.x - 1, y: 0 }),
                ScreenEdge::Right => (display.y, display.bottom(), display.right(), Point { x: display.right(), y: 0 }),
                ScreenEdge::Top => (display.x, display.right(), display.y, Point { x: 0, y: display.y - 1 }),
                ScreenEdge::Bottom => (display.x, display.right(), display.bottom(), Point { x: 0, y: display.bottom() }),
            };
            let mut blocked = Vec::new();
            for candidate in &self.displays {
                let candidate_contains_boundary = match edge {
                    ScreenEdge::Left | ScreenEdge::Right => {
                        candidate.x <= outside.x && outside.x < candidate.right()
                            && candidate.x != display.x
                    }
                    ScreenEdge::Top | ScreenEdge::Bottom => {
                        candidate.y <= outside.y && outside.y < candidate.bottom()
                            && candidate.y != display.y
                    }
                };
                if !candidate_contains_boundary {
                    continue;
                }
                let overlap_start = match edge {
                    ScreenEdge::Left | ScreenEdge::Right => display.y.max(candidate.y),
                    ScreenEdge::Top | ScreenEdge::Bottom => display.x.max(candidate.x),
                };
                let overlap_end = match edge {
                    ScreenEdge::Left | ScreenEdge::Right => display.bottom().min(candidate.bottom()),
                    ScreenEdge::Top | ScreenEdge::Bottom => display.right().min(candidate.right()),
                };
                if overlap_start < overlap_end {
                    blocked.push((overlap_start, overlap_end));
                }
            }
            blocked.sort_unstable();
            let mut cursor = start;
            for (blocked_start, blocked_end) in blocked {
                if cursor < blocked_start {
                    segments.push(EdgeSegment { start: cursor, end: blocked_start, boundary });
                }
                cursor = cursor.max(blocked_end);
            }
            if cursor < end {
                segments.push(EdgeSegment { start: cursor, end, boundary });
            }
        }
        segments.sort_by_key(|segment| (segment.start, segment.end, segment.boundary));
        let mut merged: Vec<EdgeSegment> = Vec::new();
        for segment in segments {
            if let Some(last) = merged.last_mut()
                && last.end == segment.start
                && last.boundary == segment.boundary
            {
                last.end = segment.end;
            } else {
                merged.push(segment);
            }
        }
        merged
    }
}

fn crossing_coordinate(
    coordinate: i32,
    coordinate_delta: i32,
    axis: i32,
    axis_delta: i32,
    boundary: i32,
) -> i32 {
    if axis_delta == 0 {
        return coordinate;
    }
    let progress = f64::from(boundary - axis) / f64::from(axis_delta);
    (f64::from(coordinate) + f64::from(coordinate_delta) * progress).round() as i32
}

#[derive(Clone, Copy)]
struct EdgeSegment {
    start: i32,
    end: i32,
    boundary: i32,
}

fn point_matches_boundary(edge: ScreenEdge, point: Point, boundary: i32) -> bool {
    match edge {
        ScreenEdge::Left => (point.x - boundary).abs() <= 2,
        ScreenEdge::Right => (point.x - (boundary - 1)).abs() <= 2,
        ScreenEdge::Top => (point.y - boundary).abs() <= 2,
        ScreenEdge::Bottom => (point.y - (boundary - 1)).abs() <= 2,
    }
}

#[cfg(test)]
mod tests {
    use super::{DesktopLayout, DisplayRect, Point, ScreenEdge};

    fn layout() -> DesktopLayout {
        DesktopLayout::new(vec![
            DisplayRect { x: 0, y: 0, width: 1920, height: 1080 },
            DisplayRect { x: 1920, y: 200, width: 1280, height: 1024 },
        ])
        .unwrap()
    }

    #[test]
    fn jump_zone_uses_the_outermost_pixel_without_an_outward_delta() {
        let layout = layout();
        assert!(layout.is_jump_zone_point(
            ScreenEdge::Right,
            Point { x: 3199, y: 500 },
            1,
        ));
        assert!(!layout.is_jump_zone_point(
            ScreenEdge::Right,
            Point { x: 3198, y: 500 },
            1,
        ));
        assert!(layout.is_jump_zone_point(
            ScreenEdge::Right,
            Point { x: 3200, y: 500 },
            1,
        ));
        assert!(!layout.is_jump_zone_point(
            ScreenEdge::Right,
            Point { x: 1919, y: 500 },
            1,
        ));
    }

    #[test]
    fn edge_mapping_handles_negative_coordinates() {
        let layout = DesktopLayout::new(vec![DisplayRect { x: -1200, y: -400, width: 1200, height: 900 }]).unwrap();
        let point = layout.point_inside_edge(ScreenEdge::Right, 0.5, 8);
        assert_eq!(point, Point { x: -9, y: 50 });
    }

    #[test]
    fn edge_mapping_uses_real_segments_in_irregular_layout() {
        let layout = DesktopLayout::new(vec![
            DisplayRect { x: 0, y: 0, width: 100, height: 100 },
            DisplayRect { x: 200, y: 50, width: 100, height: 100 },
        ])
        .unwrap();
        assert_eq!(
            layout.point_inside_edge(ScreenEdge::Right, 0.75, 8),
            Point { x: 291, y: 100 }
        );
    }

    #[test]
    fn edge_normalization_distinguishes_parallel_exposed_segments() {
        let layout = DesktopLayout::new(vec![
            DisplayRect { x: 0, y: 0, width: 100, height: 100 },
            DisplayRect { x: 200, y: 0, width: 100, height: 100 },
        ])
        .unwrap();
        assert_eq!(
            layout.normalized_edge_position(ScreenEdge::Right, Point { x: 299, y: 50 }),
            0.75
        );
        assert_eq!(
            layout.crossed_outer_edge_position(
                ScreenEdge::Right,
                Point { x: 291, y: 50 },
                4,
                0,
            ),
            None,
        );
        assert_eq!(
            layout.crossed_outer_edge_position(
                ScreenEdge::Right,
                Point { x: 291, y: 50 },
                12,
                0,
            ),
            Some(0.75),
        );
    }

    #[test]
    fn receiver_motion_accumulates_until_it_crosses_the_return_edge() {
        let layout = DesktopLayout::new(vec![DisplayRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }])
        .unwrap();
        let mut point = Point { x: 8, y: 50 };
        point = layout.move_within_layout(point, 30, 4);
        point = layout.move_within_layout(point, 40, 3);
        assert_eq!(point, Point { x: 78, y: 57 });
        assert_eq!(
            layout.crossed_outer_edge_position(ScreenEdge::Left, point, -90, 0),
            Some(0.57),
        );
    }

    #[test]
    fn receiver_motion_is_clamped_to_real_displays() {
        let layout = DesktopLayout::new(vec![
            DisplayRect { x: 0, y: 0, width: 100, height: 100 },
            DisplayRect { x: 200, y: 0, width: 100, height: 100 },
        ])
        .unwrap();
        assert_eq!(
            layout.move_within_layout(Point { x: 90, y: 50 }, 40, 0),
            Point { x: 99, y: 50 },
        );
    }
}
