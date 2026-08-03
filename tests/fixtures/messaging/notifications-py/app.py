"""Notification service: consumes the order events the orders service emits.

A Python twin of the JS notification fixture. Kept in Python on purpose: the
channel tests must run without `scip-typescript` (or any other external
indexer) on PATH, and Python parsing is pure tree-sitter.
"""

from kafka import KafkaConsumer

consumer = KafkaConsumer("orders.created", bootstrap_servers="localhost:9092")


def handle_order_created(message):
    """React to one order event."""
    print("notify", message.value)


def start():
    consumer.subscribe(["orders.created"])
    for message in consumer:
        handle_order_created(message)
