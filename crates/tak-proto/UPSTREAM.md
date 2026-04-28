# Upstream protobuf vendor

`.proto` files in `proto/` are vendored verbatim from the upstream
[TAK Server](https://github.com/TAK-Product-Center/Server) repository.

Do not edit these files in this tree. Use the `proto-vendor` agent or
`/proto-sync` slash command to refresh.

## Source

- Repository: `https://github.com/TAK-Product-Center/Server`
- Path: `src/takserver-protobuf/src/main/proto/`
- Vendored from local clone: `.scratch/takserver-java/`

## Last sync

- Commit SHA: `5187abd46d827d37cfc5708805eced197a837e49`
- Date:       2026-04-27 (initial vendor)

## Files (15)

| File | Package | Notes |
|------|---------|-------|
| takmessage.proto      | atakmap.commoncommo.protobuf.v1 | top-level wrapper |
| cotevent.proto        | atakmap.commoncommo.protobuf.v1 | the CoT event |
| detail.proto          | atakmap.commoncommo.protobuf.v1 | typed sub-messages + xmlDetail |
| contact.proto         | atakmap.commoncommo.protobuf.v1 | typed Detail.contact |
| group.proto           | atakmap.commoncommo.protobuf.v1 | typed Detail.group |
| precisionlocation.proto | atakmap.commoncommo.protobuf.v1 | typed Detail.precisionLocation |
| status.proto          | atakmap.commoncommo.protobuf.v1 | typed Detail.status |
| takv.proto            | atakmap.commoncommo.protobuf.v1 | typed Detail.takv |
| track.proto           | atakmap.commoncommo.protobuf.v1 | typed Detail.track |
| takcontrol.proto      | atakmap.commoncommo.protobuf.v1 | protocol negotiation |
| message.proto         | atakmap.commoncommo.protobuf.v1 | server-internal envelope |
| binarypayload.proto   | gov.tak.cop.proto.v1            | file/image attachments |
| missionannouncement.proto | atakmap.commoncommo.protobuf.v1 | mission events |
| streaminginput.proto  | com.atakmap                     | streaming input service |
| fig.proto             | com.atakmap                     | federation v2 (gRPC) |

## Wire-format-breaking changes

If a `/proto-sync` reports a wire-format-breaking change (removed field,
renumbered field, type change), STOP and escalate. Federation peers and
existing DB rows depend on the wire being stable.
