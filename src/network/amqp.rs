use env_logger;
use std::env;

use amqp::session::Options;
use amqp::session::Session;
use amqp::protocol;
use amqp::table;
use amqp::basic::Basic;
use amqp::channel::Channel;
use std::default::Default;

use std::sync::mpsc::{Sender};
use bus::{Acl, Command, Message, Tag};


pub fn init_rabbitmq(bus_tx: Sender<Message>) {
    let consumer_function = |channel: &mut Channel, deliver: protocol::basic::Deliver, headers: protocol::basic::BasicProperties, body: Vec<u8>| {
        println!("Got a delivery:");
        println!("Deliver info: {:?}", deliver);
        println!("Content headers: {:?}", headers);
        println!("Content body: {:?}", body);
        println!("Content body(as string): {:?}", String::from_utf8(body));
        channel.basic_ack(deliver.delivery_tag, false);
        bus_tx.send(Message::Msg(
            Tag::AddAcl,
            Command::AddAcl(Acl {
                app_id: String::new() + "app_e74eb0d4-e01a-4a09-af46-7ecab7157d32",
                hostname: String::new() + "cltdl.fr",
                path_begin: String::new() + ""
            })
        ));
    };

    env_logger::init().unwrap();
    let connection_uri = &env::var("AMQP_URI").ok().expect("No RabbitMQ connection provided");
    let mut session = Session::open_url(connection_uri).ok().expect("Can't create session");
    println!("Openned session");
    let mut channel = session.open_channel(1).ok().expect("Error openning channel 1");
    println!("Openned channel: {:?}", channel.id);

    let queue_name = "test_queue";
    let queue_declare = channel.queue_declare(queue_name, false, true, false, false, false, table::new());

    println!("Queue declare: {:?}", queue_declare);
    channel.basic_prefetch(10);
    println!("Declaring consumer...");
    let consumer_name = channel.basic_consume(consumer_function, queue_name, "", false, false, false, false, table::new());

    println!("Starting consumer {:?}", consumer_name);
    channel.start_consuming();

    channel.close(200, "Bye".to_string());
    session.close(200, "Good Bye".to_string());
}
