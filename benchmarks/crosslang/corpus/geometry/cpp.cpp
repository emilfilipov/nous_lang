// Cross-language geometry suite (C++). Integer geometry on Point structs.
#include <cstdint>
#include <iostream>

struct Point { std::int64_t x, y; };

std::int64_t abs_val(std::int64_t n) {
    return n < 0 ? -n : n;
}

std::int64_t dist_sq(Point a, Point b) {
    std::int64_t dx = a.x - b.x;
    std::int64_t dy = a.y - b.y;
    return dx * dx + dy * dy;
}

std::int64_t manhattan(Point a, Point b) {
    return abs_val(a.x - b.x) + abs_val(a.y - b.y);
}

std::int64_t cross(std::int64_t ax, std::int64_t ay, std::int64_t bx, std::int64_t by) {
    return ax * by - ay * bx;
}

std::int64_t dot(std::int64_t ax, std::int64_t ay, std::int64_t bx, std::int64_t by) {
    return ax * bx + ay * by;
}

std::int64_t triangle_area2(Point a, Point b, Point c) {
    return cross(b.x - a.x, b.y - a.y, c.x - a.x, c.y - a.y);
}

std::int64_t is_ccw(Point a, Point b, Point c) {
    return triangle_area2(a, b, c) > 0 ? 1 : 0;
}

std::int64_t collinear(Point a, Point b, Point c) {
    return triangle_area2(a, b, c) == 0 ? 1 : 0;
}

std::int64_t on_segment_x(Point a, Point b, std::int64_t px) {
    std::int64_t lo = a.x < b.x ? a.x : b.x;
    std::int64_t hi = a.x > b.x ? a.x : b.x;
    return (px >= lo && px <= hi) ? 1 : 0;
}

std::int64_t rect_area(std::int64_t w, std::int64_t h) {
    return w * h;
}

std::int64_t perimeter_rect(std::int64_t w, std::int64_t h) {
    return 2 * (w + h);
}

std::int64_t point_in_rect(std::int64_t px, std::int64_t py, std::int64_t w, std::int64_t h) {
    return (px >= 0 && py >= 0 && px <= w && py <= h) ? 1 : 0;
}

std::int64_t midpoint_x(Point a, Point b) {
    return (a.x + b.x) / 2;
}

std::int64_t midpoint_y(Point a, Point b) {
    return (a.y + b.y) / 2;
}

std::int64_t taxicab_circle_points(std::int64_t r) {
    return r > 0 ? 4 * r : 1;
}

std::int64_t quadrant(std::int64_t px, std::int64_t py) {
    if (px == 0 || py == 0) return 0;
    if (px > 0 && py > 0) return 1;
    if (px < 0 && py > 0) return 2;
    if (px < 0 && py < 0) return 3;
    return 4;
}

int main() {
    Point a{ 0, 0 }, b{ 3, 4 }, c{ 6, 8 };
    std::cout << "dist_sq=" << dist_sq(a, b) << "\n";
    std::cout << "manhattan=" << manhattan(a, b) << "\n";
    std::cout << "cross=" << cross(3, 4, 6, 8) << "\n";
    std::cout << "dot=" << dot(3, 4, 6, 8) << "\n";
    Point d{ 4, 0 };
    std::cout << "triangle_area2=" << triangle_area2(a, b, d) << "\n";
    std::cout << "is_ccw=" << is_ccw(a, b, d) << "\n";
    std::cout << "collinear=" << collinear(a, b, c) << "\n";
    std::cout << "on_segment_x=" << on_segment_x(a, b, 2) << "\n";
    std::cout << "rect_area=" << rect_area(3, 5) << "\n";
    std::cout << "perimeter_rect=" << perimeter_rect(3, 5) << "\n";
    std::cout << "point_in_rect=" << point_in_rect(2, 4, 3, 5) << "\n";
    std::cout << "midpoint_x=" << midpoint_x(a, c) << "\n";
    std::cout << "midpoint_y=" << midpoint_y(a, c) << "\n";
    std::cout << "taxicab_circle_points=" << taxicab_circle_points(3) << "\n";
    std::cout << "quadrant=" << quadrant(-2, 5) << "\n";
    return 0;
}
