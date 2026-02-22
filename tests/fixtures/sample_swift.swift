import Foundation
import UIKit

// A protocol defining shape behavior
protocol Shape {
    var area: Double { get }
    var perimeter: Double { get }
    func describe() -> String
}

// A simple struct
struct Point {
    var x: Double
    var y: Double

    func distanceTo(_ other: Point) -> Double {
        let dx = x - other.x
        let dy = y - other.y
        return (dx * dx + dy * dy).squareRoot()
    }
}

// A class conforming to a protocol
class Circle: Shape {
    var radius: Double

    init(radius: Double) {
        self.radius = radius
    }

    var area: Double {
        return Double.pi * radius * radius
    }

    var perimeter: Double {
        return 2 * Double.pi * radius
    }

    func describe() -> String {
        return "Circle with radius \(radius)"
    }
}

// A struct conforming to a protocol
struct Rectangle: Shape {
    var width: Double
    var height: Double

    var area: Double {
        return width * height
    }

    var perimeter: Double {
        return 2 * (width + height)
    }

    func describe() -> String {
        return "Rectangle \(width)x\(height)"
    }
}

// An enum with associated values
enum Result<T> {
    case success(T)
    case failure(Error)
}

// An actor for concurrency safety
actor Counter {
    private var count: Int = 0

    func increment() {
        count += 1
    }

    func value() -> Int {
        return count
    }
}

// Extension adding functionality
extension Circle {
    func scale(by factor: Double) -> Circle {
        return Circle(radius: radius * factor)
    }
}

// A generic function
func largestOf<T: Comparable>(_ a: T, _ b: T) -> T {
    return a > b ? a : b
}

// Type alias
typealias ShapeList = [Shape]

// A top-level function
func printShapeInfo(_ shape: Shape) {
    print(shape.describe())
    print("Area: \(shape.area)")
    print("Perimeter: \(shape.perimeter)")
}

// Main usage
let circle = Circle(radius: 5.0)
let rect = Rectangle(width: 4.0, height: 6.0)
let shapes: ShapeList = [circle, rect]

for shape in shapes {
    printShapeInfo(shape)
}

let p1 = Point(x: 0, y: 0)
let p2 = Point(x: 3, y: 4)
let distance = p1.distanceTo(p2)
print("Distance: \(distance)")

let bigger = largestOf(circle.area, rect.area)
print("Largest area: \(bigger)")
