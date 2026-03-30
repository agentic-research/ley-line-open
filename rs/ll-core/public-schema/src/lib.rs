// Public Intermediate Representation (IR) for Ley-line
// This schema allows Mache (Go) and other clients to communicate with
// the Ley-line Data Plane without direct dependency on the Private Core.

pub mod v2 {
    tonic::include_proto!("leyline.v2");
}

pub use v2::*;
