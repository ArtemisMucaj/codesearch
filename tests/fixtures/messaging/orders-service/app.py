"""Orders service: publishes order events and serves the orders API."""

from flask import Flask, jsonify
from kafka import KafkaProducer

app = Flask(__name__)
producer = KafkaProducer(bootstrap_servers="localhost:9092")


def checkout(order):
    producer.send("orders.created", order)
    return order


def audit(order):
    # No service consumes this topic — must land in the unmatched report.
    producer.send("orders.audited", order)


@app.route("/api/orders/<order_id>")
def get_order(order_id):
    return jsonify({"id": order_id, "status": "created"})
