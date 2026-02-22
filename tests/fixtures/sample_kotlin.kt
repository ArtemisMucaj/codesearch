package com.example

import kotlin.math.PI
import java.util.ArrayList

// Interface (maps to class_declaration in the grammar)
interface Shape {
    fun area(): Double
    fun perimeter(): Double
}

// Data class implementing an interface
data class Rectangle(val width: Double, val height: Double) : Shape {
    override fun area(): Double = width * height
    override fun perimeter(): Double = 2 * (width + height)
}

// Class with companion object
class Circle(val radius: Double) : Shape {
    override fun area(): Double = PI * radius * radius
    override fun perimeter(): Double = 2 * PI * radius

    companion object {
        fun fromDiameter(diameter: Double): Circle = Circle(diameter / 2)
    }
}

// Enum class
enum class Color {
    RED, GREEN, BLUE
}

// Singleton object
object MathUtils {
    fun square(x: Double): Double = x * x
    fun cube(x: Double): Double = x * x * x
}

// Top-level function
fun printShapeInfo(shape: Shape) {
    println("Area: ${shape.area()}")
    println("Perimeter: ${shape.perimeter()}")
}

// Type alias
typealias ShapeList = ArrayList<Shape>

// Extension function
fun Shape.describe(): String = "Shape with area ${area()}"
