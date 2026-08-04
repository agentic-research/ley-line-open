use capnp::message::Builder;
use leyline_public_schema::execution_capnp;

#[test]
fn generated_rust_surface_builds_run_spec_and_grant() {
    let mut spec_message = Builder::new_default();
    {
        let mut spec = spec_message.init_root::<execution_capnp::run_spec::Builder<'_>>();
        spec.set_schema_version("cloister/execution/v1");
        spec.set_cancellation_mode(execution_capnp::CancellationMode::ExplicitOnly);
    }
    let mut spec_bytes = Vec::new();
    capnp::serialize::write_message(&mut spec_bytes, &spec_message).expect("serialize RunSpec");
    let mut spec_slice = spec_bytes.as_slice();
    let spec_wire =
        capnp::serialize::read_message(&mut spec_slice, capnp::message::ReaderOptions::new())
            .expect("deserialize RunSpec");
    let spec = spec_wire
        .get_root::<execution_capnp::run_spec::Reader<'_>>()
        .expect("read generated RunSpec");
    assert_eq!(
        spec.get_schema_version()
            .expect("schemaVersion")
            .to_str()
            .expect("utf8"),
        "cloister/execution/v1"
    );

    let mut grant_message = Builder::new_default();
    {
        let mut grant = grant_message.init_root::<execution_capnp::run_grant::Builder<'_>>();
        grant.set_grant_id("grant-01");
        grant.set_backend_class(execution_capnp::BackendClass::MicroVm);
    }
    let mut grant_bytes = Vec::new();
    capnp::serialize::write_message(&mut grant_bytes, &grant_message).expect("serialize RunGrant");
    let mut grant_slice = grant_bytes.as_slice();
    let grant_wire =
        capnp::serialize::read_message(&mut grant_slice, capnp::message::ReaderOptions::new())
            .expect("deserialize RunGrant");
    let grant = grant_wire
        .get_root::<execution_capnp::run_grant::Reader<'_>>()
        .expect("read generated RunGrant");
    assert_eq!(
        grant.get_backend_class().expect("known backend"),
        execution_capnp::BackendClass::MicroVm
    );

    let mut status_message = Builder::new_default();
    {
        let mut status = status_message.init_root::<execution_capnp::status_output::Builder<'_>>();
        status.set_provisioned(false);
        status.set_backend("");
        status.set_state(execution_capnp::RunState::Accepted);
    }
    let mut status_bytes = Vec::new();
    capnp::serialize::write_message(&mut status_bytes, &status_message)
        .expect("serialize StatusOutput");
    let mut status_slice = status_bytes.as_slice();
    let status_wire =
        capnp::serialize::read_message(&mut status_slice, capnp::message::ReaderOptions::new())
            .expect("deserialize StatusOutput");
    let status = status_wire
        .get_root::<execution_capnp::status_output::Reader<'_>>()
        .expect("read generated StatusOutput");
    assert!(
        !status.get_provisioned(),
        "status does not imply provisioning"
    );

    let mut receipt_message = Builder::new_default();
    {
        let mut receipt = receipt_message.init_root::<execution_capnp::run_receipt::Builder<'_>>();
        receipt.set_schema_version("cloister/execution/v1");
        receipt.set_run_id("run-01");
        receipt.set_terminal_state(execution_capnp::RunState::Succeeded);
    }
    let mut receipt_bytes = Vec::new();
    capnp::serialize::write_message(&mut receipt_bytes, &receipt_message)
        .expect("serialize RunReceipt");
    let mut receipt_slice = receipt_bytes.as_slice();
    let receipt_wire =
        capnp::serialize::read_message(&mut receipt_slice, capnp::message::ReaderOptions::new())
            .expect("deserialize RunReceipt");
    let receipt = receipt_wire
        .get_root::<execution_capnp::run_receipt::Reader<'_>>()
        .expect("read generated RunReceipt");
    assert_eq!(
        receipt.get_terminal_state().expect("known state"),
        execution_capnp::RunState::Succeeded
    );
}
