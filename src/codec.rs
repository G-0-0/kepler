use rocket::{
    data::{DataStream, ToByteUnit},
    form::{DataField, FromFormField, Result, ValueField},
    http::ContentType,
    request::{FromRequest, Outcome, Request},
};

pub enum SupportedCodecs {
    Raw = 0x55,
    Json = 0x0200,
    MsgPack = 0x0201,
    Cbor = 0x51,
}

pub struct PutContent {
    pub codec: SupportedCodecs,
    pub content: DataStream,
}

impl From<&ContentType> for SupportedCodecs {
    fn from(c: &ContentType) -> Self {
        if c.is_json() {
            Self::Json
        } else if c.is_msgpack() {
            Self::MsgPack
        } else {
            Self::Raw
        }
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for SupportedCodecs {
    type Error = anyhow::Error;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        Outcome::Success(match req.content_type() {
            Some(t) => Self::from(t),
            None => Self::Raw,
        })
    }
}

#[rocket::async_trait]
impl<'r> FromFormField<'r> for PutContent {
    fn from_value(field: ValueField<'r>) -> Result<'r, Self> {
        todo!()
    }

    async fn from_data(field: DataField<'r, '_>) -> Result<'r, Self> {
        Ok(PutContent {
            codec: (&field.content_type).into(),
            content: field.data.open(1.megabytes()),
        })
    }
}
