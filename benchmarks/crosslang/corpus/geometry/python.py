"""Cross-language geometry suite (Python). Integer geometry on Point structs."""
from dataclasses import dataclass


@dataclass
class Point:
    x: int
    y: int


def dist_sq(a: Point, b: Point) -> int:
    dx = a.x - b.x
    dy = a.y - b.y
    return dx * dx + dy * dy


def manhattan(a: Point, b: Point) -> int:
    return abs(a.x - b.x) + abs(a.y - b.y)


def cross(ax: int, ay: int, bx: int, by: int) -> int:
    return ax * by - ay * bx


def dot(ax: int, ay: int, bx: int, by: int) -> int:
    return ax * bx + ay * by


def triangle_area2(a: Point, b: Point, c: Point) -> int:
    return cross(b.x - a.x, b.y - a.y, c.x - a.x, c.y - a.y)


def is_ccw(a: Point, b: Point, c: Point) -> int:
    return 1 if triangle_area2(a, b, c) > 0 else 0


def collinear(a: Point, b: Point, c: Point) -> int:
    return 1 if triangle_area2(a, b, c) == 0 else 0


def on_segment_x(a: Point, b: Point, px: int) -> int:
    lo = min(a.x, b.x)
    hi = max(a.x, b.x)
    return 1 if lo <= px <= hi else 0


def rect_area(w: int, h: int) -> int:
    return w * h


def perimeter_rect(w: int, h: int) -> int:
    return 2 * (w + h)


def point_in_rect(px: int, py: int, w: int, h: int) -> int:
    return 1 if 0 <= px <= w and 0 <= py <= h else 0


def midpoint_x(a: Point, b: Point) -> int:
    return (a.x + b.x) // 2


def midpoint_y(a: Point, b: Point) -> int:
    return (a.y + b.y) // 2


def taxicab_circle_points(r: int) -> int:
    return 4 * r if r > 0 else 1


def quadrant(px: int, py: int) -> int:
    if px == 0 or py == 0:
        return 0
    if px > 0 and py > 0:
        return 1
    if px < 0 and py > 0:
        return 2
    if px < 0 and py < 0:
        return 3
    return 4


def main() -> None:
    a, b, c = Point(0, 0), Point(3, 4), Point(6, 8)
    print("dist_sq=" + str(dist_sq(a, b)))
    print("manhattan=" + str(manhattan(a, b)))
    print("cross=" + str(cross(3, 4, 6, 8)))
    print("dot=" + str(dot(3, 4, 6, 8)))
    d = Point(4, 0)
    print("triangle_area2=" + str(triangle_area2(a, b, d)))
    print("is_ccw=" + str(is_ccw(a, b, d)))
    print("collinear=" + str(collinear(a, b, c)))
    print("on_segment_x=" + str(on_segment_x(a, b, 2)))
    print("rect_area=" + str(rect_area(3, 5)))
    print("perimeter_rect=" + str(perimeter_rect(3, 5)))
    print("point_in_rect=" + str(point_in_rect(2, 4, 3, 5)))
    print("midpoint_x=" + str(midpoint_x(a, c)))
    print("midpoint_y=" + str(midpoint_y(a, c)))
    print("taxicab_circle_points=" + str(taxicab_circle_points(3)))
    print("quadrant=" + str(quadrant(-2, 5)))


if __name__ == "__main__":
    main()
