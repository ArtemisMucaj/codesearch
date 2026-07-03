// Notification service: consumes order events and enriches them via the
// orders API.
const { Kafka } = require('kafkajs');
const axios = require('axios');

const kafka = new Kafka({ clientId: 'notifications', brokers: ['localhost:9092'] });
const consumer = kafka.consumer({ groupId: 'notifications' });

async function start() {
  await consumer.subscribe({ topics: ['orders.created'] });
  await consumer.run({ eachMessage: handleOrderCreated });
}

async function handleOrderCreated({ message }) {
  const order = JSON.parse(message.value.toString());
  const details = await fetchOrder(order.id);
  console.log('notify', details.status);
}

async function fetchOrder(orderId) {
  const res = await axios.get('http://orders-service/api/orders/123');
  return res.data;
}

module.exports = { start };
