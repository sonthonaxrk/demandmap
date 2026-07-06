

#[test]
fn req() {
    let mut resp = ureq::head("https://rollo-testing.lon1.digitaloceanspaces.com/my_large_thing")
        .header("Range", "0-4096")
        .call()
        .unwrap();

    let length = resp
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    dbg!(length);

    //
    // let mut builder = ureq::get("https://rollo-testing.lon1.digitaloceanspaces.com/my_large_thing")
    //     .header("Range", "0-4096")
    //     .call().unwrap();
    //
    // let mut res = builder.body_mut();
    // let mut read= res.as_reader();
    //
    // let mut data = Box::new([0u8;4096]);
    // read.read(&mut *data);
    //
    //
    //
    // dbg!(data.hex_dump());
}
