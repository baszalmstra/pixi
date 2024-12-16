use crate::DeserializeAs;
use std::marker::PhantomData;
use toml_span::{DeserError, Deserialize, Value};

pub struct TomlWith<T, U> {
    value: T,
    _data: PhantomData<U>,
}

impl<'de, T, U> Deserialize<'de> for TomlWith<T, U>
where
    U: DeserializeAs<'de, T>,
{
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        U::deserialize_as(value).map(|value| TomlWith {
            value,
            _data: PhantomData,
        })
    }
}

impl<T, U> TomlWith<T, U> {
    pub fn into_inner(self) -> T {
        self.value
    }
}
