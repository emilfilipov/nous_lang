// Cross-language geometry suite (Rust). Integer geometry on Point structs.

#[derive(Clone, Copy)]
struct Point { x: i64, y: i64 }

fn dist_sq(a: Point, b: Point) -> i64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    dx * dx + dy * dy
}

fn manhattan(a: Point, b: Point) -> i64 {
    (a.x - b.x).abs() + (a.y - b.y).abs()
}

fn cross(ax: i64, ay: i64, bx: i64, by: i64) -> i64 {
    ax * by - ay * bx
}

fn dot(ax: i64, ay: i64, bx: i64, by: i64) -> i64 {
    ax * bx + ay * by
}

fn triangle_area2(a: Point, b: Point, c: Point) -> i64 {
    cross(b.x - a.x, b.y - a.y, c.x - a.x, c.y - a.y)
}

fn is_ccw(a: Point, b: Point, c: Point) -> i64 {
    if triangle_area2(a, b, c) > 0 { 1 } else { 0 }
}

fn collinear(a: Point, b: Point, c: Point) -> i64 {
    if triangle_area2(a, b, c) == 0 { 1 } else { 0 }
}

fn on_segment_x(a: Point, b: Point, px: i64) -> i64 {
    let lo = a.x.min(b.x);
    let hi = a.x.max(b.x);
    if px >= lo && px <= hi { 1 } else { 0 }
}

fn rect_area(w: i64, h: i64) -> i64 {
    w * h
}

fn perimeter_rect(w: i64, h: i64) -> i64 {
    2 * (w + h)
}

fn point_in_rect(px: i64, py: i64, w: i64, h: i64) -> i64 {
    if px >= 0 && py >= 0 && px <= w && py <= h { 1 } else { 0 }
}

fn midpoint_x(a: Point, b: Point) -> i64 {
    (a.x + b.x) / 2
}

fn midpoint_y(a: Point, b: Point) -> i64 {
    (a.y + b.y) / 2
}

fn taxicab_circle_points(r: i64) -> i64 {
    if r > 0 { 4 * r } else { 1 }
}

fn quadrant(px: i64, py: i64) -> i64 {
    if px == 0 || py == 0 { return 0; }
    if px > 0 && py > 0 { return 1; }
    if px < 0 && py > 0 { return 2; }
    if px < 0 && py < 0 { return 3; }
    4
}

fn main() {
    let a = Point { x: 0, y: 0 };
    let b = Point { x: 3, y: 4 };
    let c = Point { x: 6, y: 8 };
    println!("dist_sq={}", dist_sq(a, b));
    println!("manhattan={}", manhattan(a, b));
    println!("cross={}", cross(3, 4, 6, 8));
    println!("dot={}", dot(3, 4, 6, 8));
    let d = Point { x: 4, y: 0 };
    println!("triangle_area2={}", triangle_area2(a, b, d));
    println!("is_ccw={}", is_ccw(a, b, d));
    println!("collinear={}", collinear(a, b, c));
    println!("on_segment_x={}", on_segment_x(a, b, 2));
    println!("rect_area={}", rect_area(3, 5));
    println!("perimeter_rect={}", perimeter_rect(3, 5));
    println!("point_in_rect={}", point_in_rect(2, 4, 3, 5));
    println!("midpoint_x={}", midpoint_x(a, c));
    println!("midpoint_y={}", midpoint_y(a, c));
    println!("taxicab_circle_points={}", taxicab_circle_points(3));
    println!("quadrant={}", quadrant(-2, 5));
}
