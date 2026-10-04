//! Real application protocols transported through a Sōzu TCP listener.

pub(super) mod fixture;

#[cfg(feature = "service-kafka")]
mod kafka;
#[cfg(feature = "service-mongodb")]
mod mongodb;
#[cfg(feature = "service-mysql")]
mod mysql;
#[cfg(feature = "service-postgres")]
mod postgres;
#[cfg(feature = "service-pulsar")]
mod pulsar;
#[cfg(feature = "service-rabbitmq")]
mod rabbitmq;
#[cfg(feature = "service-redis")]
mod redis;
