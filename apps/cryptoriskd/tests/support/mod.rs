use local_api::proto::market_v1::{GetSnapshotRequest, GetSnapshotResponse};
use tonic::{
    IntoRequest, Response, Status,
    client::Grpc,
    codegen::http::uri::PathAndQuery,
    transport::{Channel, Endpoint},
};

pub struct MarketTestClient {
    inner: Grpc<Channel>,
}

impl MarketTestClient {
    pub async fn connect(endpoint: String) -> Self {
        let channel = Endpoint::from_shared(endpoint)
            .expect("loopback endpoint must be valid")
            .connect()
            .await
            .expect("loopback RPC must connect");
        Self {
            inner: Grpc::new(channel),
        }
    }

    pub async fn get_snapshot(
        &mut self,
        request: impl IntoRequest<GetSnapshotRequest>,
    ) -> Result<Response<GetSnapshotResponse>, Status> {
        self.inner
            .ready()
            .await
            .map_err(|_| Status::unavailable("loopback transport unavailable"))?;
        self.inner
            .unary(
                request.into_request(),
                PathAndQuery::from_static("/cmti.market.v1.MarketService/GetSnapshot"),
                tonic_prost::ProstCodec::default(),
            )
            .await
    }
}
