#![allow(dead_code)]
#![warn(clippy::anonymous_tuple_return_type)]

struct Dimensions {
    width: u32,
    height: u32,
}

struct TupleStruct(u32, u32);

type DimensionPair = (u32, u32);

fn direct_tuple() -> (u32, String) {
    //~^ anonymous_tuple_return_type
    (1, String::new())
}

fn single_element_tuple() -> (u32,) {
    //~^ anonymous_tuple_return_type
    (1,)
}

fn nested_tuple() -> Result<(u32, String), String> {
    //~^ anonymous_tuple_return_type
    Ok((1, String::new()))
}

fn named_struct() -> Dimensions {
    Dimensions { width: 1, height: 2 }
}

fn tuple_struct() -> TupleStruct {
    TupleStruct(1, 2)
}

fn type_alias() -> DimensionPair {
    (1, 2)
}

async fn async_tuple() -> (u32, String) {
    //~^ anonymous_tuple_return_type
    (1, String::new())
}

fn function_pointer_return() -> fn() -> (u32, String) {
    //~^ anonymous_tuple_return_type
    direct_tuple
}

trait Trait {
    fn required() -> (u32, String);
    //~^ anonymous_tuple_return_type

    fn provided() -> Option<(u32, String)> {
        //~^ anonymous_tuple_return_type
        Some((1, String::new()))
    }
}

struct Impl;

impl Impl {
    fn method() -> (u32, String) {
        //~^ anonymous_tuple_return_type
        (1, String::new())
    }
}

impl Trait for Impl {
    fn required() -> (u32, String) {
        (1, String::new())
    }
}

fn main() {
    let _closure = || -> (u32, String) {
        //~^ anonymous_tuple_return_type
        (1, String::new())
    };
}
