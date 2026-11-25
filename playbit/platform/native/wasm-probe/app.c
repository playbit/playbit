#include <playbit/sys/abi.h>
#include <stddef.h>
_Alignas(8) static uint8_t storage[4096];
uint32_t probe_base(void) { return (uint32_t)(uintptr_t)storage; }
// Compare every field offset and width with the independently compiled Rust inventory.
uint32_t probe_layout(uint32_t index) {
    const uint32_t layout[] = {
        sizeof(PBEntryHandle),
        _Alignof(PBEntryHandle),
        5,
        offsetof(PBEntryHandle, size),
        sizeof(((PBEntryHandle*)0)->size),
        offsetof(PBEntryHandle, written),
        sizeof(((PBEntryHandle*)0)->written),
        offsetof(PBEntryHandle, volume),
        sizeof(((PBEntryHandle*)0)->volume),
        offsetof(PBEntryHandle, entry_id),
        sizeof(((PBEntryHandle*)0)->entry_id),
        offsetof(PBEntryHandle, reserved),
        sizeof(((PBEntryHandle*)0)->reserved),
        sizeof(PBRange),
        _Alignof(PBRange),
        4,
        offsetof(PBRange, size),
        sizeof(((PBRange*)0)->size),
        offsetof(PBRange, written),
        sizeof(((PBRange*)0)->written),
        offsetof(PBRange, offset),
        sizeof(((PBRange*)0)->offset),
        offsetof(PBRange, count),
        sizeof(((PBRange*)0)->count),
        sizeof(PBVolume),
        _Alignof(PBVolume),
        13,
        offsetof(PBVolume, size),
        sizeof(((PBVolume*)0)->size),
        offsetof(PBVolume, written),
        sizeof(((PBVolume*)0)->written),
        offsetof(PBVolume, handle),
        sizeof(((PBVolume*)0)->handle),
        offsetof(PBVolume, id),
        sizeof(((PBVolume*)0)->id),
        offsetof(PBVolume, name_offset),
        sizeof(((PBVolume*)0)->name_offset),
        offsetof(PBVolume, name_count),
        sizeof(((PBVolume*)0)->name_count),
        offsetof(PBVolume, tags_offset),
        sizeof(((PBVolume*)0)->tags_offset),
        offsetof(PBVolume, tags_count),
        sizeof(((PBVolume*)0)->tags_count),
        offsetof(PBVolume, rights),
        sizeof(((PBVolume*)0)->rights),
        offsetof(PBVolume, flags),
        sizeof(((PBVolume*)0)->flags),
        offsetof(PBVolume, file),
        sizeof(((PBVolume*)0)->file),
        offsetof(PBVolume, grant_id),
        sizeof(((PBVolume*)0)->grant_id),
        offsetof(PBVolume, role),
        sizeof(((PBVolume*)0)->role),
        sizeof(PBVolumes),
        _Alignof(PBVolumes),
        14,
        offsetof(PBVolumes, size),
        sizeof(((PBVolumes*)0)->size),
        offsetof(PBVolumes, written),
        sizeof(((PBVolumes*)0)->written),
        offsetof(PBVolumes, revision),
        sizeof(((PBVolumes*)0)->revision),
        offsetof(PBVolumes, volumes_data),
        sizeof(((PBVolumes*)0)->volumes_data),
        offsetof(PBVolumes, volumes_capacity),
        sizeof(((PBVolumes*)0)->volumes_capacity),
        offsetof(PBVolumes, volumes_count),
        sizeof(((PBVolumes*)0)->volumes_count),
        offsetof(PBVolumes, volumes_stride),
        sizeof(((PBVolumes*)0)->volumes_stride),
        offsetof(PBVolumes, tags_data),
        sizeof(((PBVolumes*)0)->tags_data),
        offsetof(PBVolumes, tags_capacity),
        sizeof(((PBVolumes*)0)->tags_capacity),
        offsetof(PBVolumes, tags_count),
        sizeof(((PBVolumes*)0)->tags_count),
        offsetof(PBVolumes, tags_stride),
        sizeof(((PBVolumes*)0)->tags_stride),
        offsetof(PBVolumes, bytes_data),
        sizeof(((PBVolumes*)0)->bytes_data),
        offsetof(PBVolumes, bytes_capacity),
        sizeof(((PBVolumes*)0)->bytes_capacity),
        offsetof(PBVolumes, bytes_count),
        sizeof(((PBVolumes*)0)->bytes_count),
        sizeof(PBValueInput),
        _Alignof(PBValueInput),
        10,
        offsetof(PBValueInput, size),
        sizeof(((PBValueInput*)0)->size),
        offsetof(PBValueInput, written),
        sizeof(((PBValueInput*)0)->written),
        offsetof(PBValueInput, kind),
        sizeof(((PBValueInput*)0)->kind),
        offsetof(PBValueInput, length),
        sizeof(((PBValueInput*)0)->length),
        offsetof(PBValueInput, scalar),
        sizeof(((PBValueInput*)0)->scalar),
        offsetof(PBValueInput, id1),
        sizeof(((PBValueInput*)0)->id1),
        offsetof(PBValueInput, id2),
        sizeof(((PBValueInput*)0)->id2),
        offsetof(PBValueInput, entry_id),
        sizeof(((PBValueInput*)0)->entry_id),
        offsetof(PBValueInput, reserved),
        sizeof(((PBValueInput*)0)->reserved),
        offsetof(PBValueInput, data),
        sizeof(((PBValueInput*)0)->data),
        sizeof(PBFieldInput),
        _Alignof(PBFieldInput),
        7,
        offsetof(PBFieldInput, size),
        sizeof(((PBFieldInput*)0)->size),
        offsetof(PBFieldInput, written),
        sizeof(((PBFieldInput*)0)->written),
        offsetof(PBFieldInput, id),
        sizeof(((PBFieldInput*)0)->id),
        offsetof(PBFieldInput, values),
        sizeof(((PBFieldInput*)0)->values),
        offsetof(PBFieldInput, count),
        sizeof(((PBFieldInput*)0)->count),
        offsetof(PBFieldInput, stride),
        sizeof(((PBFieldInput*)0)->stride),
        offsetof(PBFieldInput, present),
        sizeof(((PBFieldInput*)0)->present),
        sizeof(PBValue),
        _Alignof(PBValue),
        11,
        offsetof(PBValue, size),
        sizeof(((PBValue*)0)->size),
        offsetof(PBValue, written),
        sizeof(((PBValue*)0)->written),
        offsetof(PBValue, kind),
        sizeof(((PBValue*)0)->kind),
        offsetof(PBValue, reserved),
        sizeof(((PBValue*)0)->reserved),
        offsetof(PBValue, scalar),
        sizeof(((PBValue*)0)->scalar),
        offsetof(PBValue, id1),
        sizeof(((PBValue*)0)->id1),
        offsetof(PBValue, id2),
        sizeof(((PBValue*)0)->id2),
        offsetof(PBValue, entry_id),
        sizeof(((PBValue*)0)->entry_id),
        offsetof(PBValue, entry_reserved),
        sizeof(((PBValue*)0)->entry_reserved),
        offsetof(PBValue, bytes_offset),
        sizeof(((PBValue*)0)->bytes_offset),
        offsetof(PBValue, bytes_count),
        sizeof(((PBValue*)0)->bytes_count),
        sizeof(PBField),
        _Alignof(PBField),
        7,
        offsetof(PBField, size),
        sizeof(((PBField*)0)->size),
        offsetof(PBField, written),
        sizeof(((PBField*)0)->written),
        offsetof(PBField, id),
        sizeof(((PBField*)0)->id),
        offsetof(PBField, values_offset),
        sizeof(((PBField*)0)->values_offset),
        offsetof(PBField, values_count),
        sizeof(((PBField*)0)->values_count),
        offsetof(PBField, present),
        sizeof(((PBField*)0)->present),
        offsetof(PBField, reserved),
        sizeof(((PBField*)0)->reserved),
        sizeof(PBObject),
        _Alignof(PBObject),
        14,
        offsetof(PBObject, size),
        sizeof(((PBObject*)0)->size),
        offsetof(PBObject, written),
        sizeof(((PBObject*)0)->written),
        offsetof(PBObject, type_id),
        sizeof(((PBObject*)0)->type_id),
        offsetof(PBObject, fields_data),
        sizeof(((PBObject*)0)->fields_data),
        offsetof(PBObject, fields_capacity),
        sizeof(((PBObject*)0)->fields_capacity),
        offsetof(PBObject, fields_count),
        sizeof(((PBObject*)0)->fields_count),
        offsetof(PBObject, fields_stride),
        sizeof(((PBObject*)0)->fields_stride),
        offsetof(PBObject, values_data),
        sizeof(((PBObject*)0)->values_data),
        offsetof(PBObject, values_capacity),
        sizeof(((PBObject*)0)->values_capacity),
        offsetof(PBObject, values_count),
        sizeof(((PBObject*)0)->values_count),
        offsetof(PBObject, values_stride),
        sizeof(((PBObject*)0)->values_stride),
        offsetof(PBObject, bytes_data),
        sizeof(((PBObject*)0)->bytes_data),
        offsetof(PBObject, bytes_capacity),
        sizeof(((PBObject*)0)->bytes_capacity),
        offsetof(PBObject, bytes_count),
        sizeof(((PBObject*)0)->bytes_count),
        sizeof(PBQuery),
        _Alignof(PBQuery),
        9,
        offsetof(PBQuery, size),
        sizeof(((PBQuery*)0)->size),
        offsetof(PBQuery, written),
        sizeof(((PBQuery*)0)->written),
        offsetof(PBQuery, type_id),
        sizeof(((PBQuery*)0)->type_id),
        offsetof(PBQuery, after),
        sizeof(((PBQuery*)0)->after),
        offsetof(PBQuery, flags),
        sizeof(((PBQuery*)0)->flags),
        offsetof(PBQuery, limit),
        sizeof(((PBQuery*)0)->limit),
        offsetof(PBQuery, fields),
        sizeof(((PBQuery*)0)->fields),
        offsetof(PBQuery, count),
        sizeof(((PBQuery*)0)->count),
        offsetof(PBQuery, reserved),
        sizeof(((PBQuery*)0)->reserved),
        sizeof(PBObjectInfo),
        _Alignof(PBObjectInfo),
        6,
        offsetof(PBObjectInfo, size),
        sizeof(((PBObjectInfo*)0)->size),
        offsetof(PBObjectInfo, written),
        sizeof(((PBObjectInfo*)0)->written),
        offsetof(PBObjectInfo, id),
        sizeof(((PBObjectInfo*)0)->id),
        offsetof(PBObjectInfo, type_id),
        sizeof(((PBObjectInfo*)0)->type_id),
        offsetof(PBObjectInfo, fields_offset),
        sizeof(((PBObjectInfo*)0)->fields_offset),
        offsetof(PBObjectInfo, fields_count),
        sizeof(((PBObjectInfo*)0)->fields_count),
        sizeof(PBPage),
        _Alignof(PBPage),
        20,
        offsetof(PBPage, size),
        sizeof(((PBPage*)0)->size),
        offsetof(PBPage, written),
        sizeof(((PBPage*)0)->written),
        offsetof(PBPage, next),
        sizeof(((PBPage*)0)->next),
        offsetof(PBPage, has_next),
        sizeof(((PBPage*)0)->has_next),
        offsetof(PBPage, projected),
        sizeof(((PBPage*)0)->projected),
        offsetof(PBPage, objects_data),
        sizeof(((PBPage*)0)->objects_data),
        offsetof(PBPage, objects_capacity),
        sizeof(((PBPage*)0)->objects_capacity),
        offsetof(PBPage, objects_count),
        sizeof(((PBPage*)0)->objects_count),
        offsetof(PBPage, objects_stride),
        sizeof(((PBPage*)0)->objects_stride),
        offsetof(PBPage, fields_data),
        sizeof(((PBPage*)0)->fields_data),
        offsetof(PBPage, fields_capacity),
        sizeof(((PBPage*)0)->fields_capacity),
        offsetof(PBPage, fields_count),
        sizeof(((PBPage*)0)->fields_count),
        offsetof(PBPage, fields_stride),
        sizeof(((PBPage*)0)->fields_stride),
        offsetof(PBPage, values_data),
        sizeof(((PBPage*)0)->values_data),
        offsetof(PBPage, values_capacity),
        sizeof(((PBPage*)0)->values_capacity),
        offsetof(PBPage, values_count),
        sizeof(((PBPage*)0)->values_count),
        offsetof(PBPage, values_stride),
        sizeof(((PBPage*)0)->values_stride),
        offsetof(PBPage, bytes_data),
        sizeof(((PBPage*)0)->bytes_data),
        offsetof(PBPage, bytes_capacity),
        sizeof(((PBPage*)0)->bytes_capacity),
        offsetof(PBPage, bytes_count),
        sizeof(((PBPage*)0)->bytes_count),
        sizeof(PBSchemaFieldInput),
        _Alignof(PBSchemaFieldInput),
        8,
        offsetof(PBSchemaFieldInput, size),
        sizeof(((PBSchemaFieldInput*)0)->size),
        offsetof(PBSchemaFieldInput, written),
        sizeof(((PBSchemaFieldInput*)0)->written),
        offsetof(PBSchemaFieldInput, id),
        sizeof(((PBSchemaFieldInput*)0)->id),
        offsetof(PBSchemaFieldInput, name_data),
        sizeof(((PBSchemaFieldInput*)0)->name_data),
        offsetof(PBSchemaFieldInput, name_length),
        sizeof(((PBSchemaFieldInput*)0)->name_length),
        offsetof(PBSchemaFieldInput, kind),
        sizeof(((PBSchemaFieldInput*)0)->kind),
        offsetof(PBSchemaFieldInput, flags),
        sizeof(((PBSchemaFieldInput*)0)->flags),
        offsetof(PBSchemaFieldInput, reference_type),
        sizeof(((PBSchemaFieldInput*)0)->reference_type),
        sizeof(PBSchemaTypeInput),
        _Alignof(PBSchemaTypeInput),
        9,
        offsetof(PBSchemaTypeInput, size),
        sizeof(((PBSchemaTypeInput*)0)->size),
        offsetof(PBSchemaTypeInput, written),
        sizeof(((PBSchemaTypeInput*)0)->written),
        offsetof(PBSchemaTypeInput, id),
        sizeof(((PBSchemaTypeInput*)0)->id),
        offsetof(PBSchemaTypeInput, name_data),
        sizeof(((PBSchemaTypeInput*)0)->name_data),
        offsetof(PBSchemaTypeInput, name_length),
        sizeof(((PBSchemaTypeInput*)0)->name_length),
        offsetof(PBSchemaTypeInput, fields),
        sizeof(((PBSchemaTypeInput*)0)->fields),
        offsetof(PBSchemaTypeInput, count),
        sizeof(((PBSchemaTypeInput*)0)->count),
        offsetof(PBSchemaTypeInput, stride),
        sizeof(((PBSchemaTypeInput*)0)->stride),
        offsetof(PBSchemaTypeInput, flags),
        sizeof(((PBSchemaTypeInput*)0)->flags),
        sizeof(PBSchemaField),
        _Alignof(PBSchemaField),
        8,
        offsetof(PBSchemaField, size),
        sizeof(((PBSchemaField*)0)->size),
        offsetof(PBSchemaField, written),
        sizeof(((PBSchemaField*)0)->written),
        offsetof(PBSchemaField, id),
        sizeof(((PBSchemaField*)0)->id),
        offsetof(PBSchemaField, name_offset),
        sizeof(((PBSchemaField*)0)->name_offset),
        offsetof(PBSchemaField, name_count),
        sizeof(((PBSchemaField*)0)->name_count),
        offsetof(PBSchemaField, kind),
        sizeof(((PBSchemaField*)0)->kind),
        offsetof(PBSchemaField, flags),
        sizeof(((PBSchemaField*)0)->flags),
        offsetof(PBSchemaField, reference_type),
        sizeof(((PBSchemaField*)0)->reference_type),
        sizeof(PBSchemaType),
        _Alignof(PBSchemaType),
        9,
        offsetof(PBSchemaType, size),
        sizeof(((PBSchemaType*)0)->size),
        offsetof(PBSchemaType, written),
        sizeof(((PBSchemaType*)0)->written),
        offsetof(PBSchemaType, id),
        sizeof(((PBSchemaType*)0)->id),
        offsetof(PBSchemaType, name_offset),
        sizeof(((PBSchemaType*)0)->name_offset),
        offsetof(PBSchemaType, name_count),
        sizeof(((PBSchemaType*)0)->name_count),
        offsetof(PBSchemaType, fields_offset),
        sizeof(((PBSchemaType*)0)->fields_offset),
        offsetof(PBSchemaType, fields_count),
        sizeof(((PBSchemaType*)0)->fields_count),
        offsetof(PBSchemaType, flags),
        sizeof(((PBSchemaType*)0)->flags),
        offsetof(PBSchemaType, reserved),
        sizeof(((PBSchemaType*)0)->reserved),
        sizeof(PBSchema),
        _Alignof(PBSchema),
        13,
        offsetof(PBSchema, size),
        sizeof(((PBSchema*)0)->size),
        offsetof(PBSchema, written),
        sizeof(((PBSchema*)0)->written),
        offsetof(PBSchema, types_data),
        sizeof(((PBSchema*)0)->types_data),
        offsetof(PBSchema, types_capacity),
        sizeof(((PBSchema*)0)->types_capacity),
        offsetof(PBSchema, types_count),
        sizeof(((PBSchema*)0)->types_count),
        offsetof(PBSchema, types_stride),
        sizeof(((PBSchema*)0)->types_stride),
        offsetof(PBSchema, fields_data),
        sizeof(((PBSchema*)0)->fields_data),
        offsetof(PBSchema, fields_capacity),
        sizeof(((PBSchema*)0)->fields_capacity),
        offsetof(PBSchema, fields_count),
        sizeof(((PBSchema*)0)->fields_count),
        offsetof(PBSchema, fields_stride),
        sizeof(((PBSchema*)0)->fields_stride),
        offsetof(PBSchema, bytes_data),
        sizeof(((PBSchema*)0)->bytes_data),
        offsetof(PBSchema, bytes_capacity),
        sizeof(((PBSchema*)0)->bytes_capacity),
        offsetof(PBSchema, bytes_count),
        sizeof(((PBSchema*)0)->bytes_count),
        sizeof(PBCapabilities),
        _Alignof(PBCapabilities),
        9,
        offsetof(PBCapabilities, size),
        sizeof(((PBCapabilities*)0)->size),
        offsetof(PBCapabilities, written),
        sizeof(((PBCapabilities*)0)->written),
        offsetof(PBCapabilities, names_data),
        sizeof(((PBCapabilities*)0)->names_data),
        offsetof(PBCapabilities, names_capacity),
        sizeof(((PBCapabilities*)0)->names_capacity),
        offsetof(PBCapabilities, names_count),
        sizeof(((PBCapabilities*)0)->names_count),
        offsetof(PBCapabilities, names_stride),
        sizeof(((PBCapabilities*)0)->names_stride),
        offsetof(PBCapabilities, bytes_data),
        sizeof(((PBCapabilities*)0)->bytes_data),
        offsetof(PBCapabilities, bytes_capacity),
        sizeof(((PBCapabilities*)0)->bytes_capacity),
        offsetof(PBCapabilities, bytes_count),
        sizeof(((PBCapabilities*)0)->bytes_count),
        sizeof(PBArgument),
        _Alignof(PBArgument),
        6,
        offsetof(PBArgument, size),
        sizeof(((PBArgument*)0)->size),
        offsetof(PBArgument, written),
        sizeof(((PBArgument*)0)->written),
        offsetof(PBArgument, name_offset),
        sizeof(((PBArgument*)0)->name_offset),
        offsetof(PBArgument, name_count),
        sizeof(((PBArgument*)0)->name_count),
        offsetof(PBArgument, value_offset),
        sizeof(((PBArgument*)0)->value_offset),
        offsetof(PBArgument, value_count),
        sizeof(((PBArgument*)0)->value_count),
        sizeof(PBArguments),
        _Alignof(PBArguments),
        9,
        offsetof(PBArguments, size),
        sizeof(((PBArguments*)0)->size),
        offsetof(PBArguments, written),
        sizeof(((PBArguments*)0)->written),
        offsetof(PBArguments, arguments_data),
        sizeof(((PBArguments*)0)->arguments_data),
        offsetof(PBArguments, arguments_capacity),
        sizeof(((PBArguments*)0)->arguments_capacity),
        offsetof(PBArguments, arguments_count),
        sizeof(((PBArguments*)0)->arguments_count),
        offsetof(PBArguments, arguments_stride),
        sizeof(((PBArguments*)0)->arguments_stride),
        offsetof(PBArguments, bytes_data),
        sizeof(((PBArguments*)0)->bytes_data),
        offsetof(PBArguments, bytes_capacity),
        sizeof(((PBArguments*)0)->bytes_capacity),
        offsetof(PBArguments, bytes_count),
        sizeof(((PBArguments*)0)->bytes_count),
        sizeof(PBParticipant),
        _Alignof(PBParticipant),
        7,
        offsetof(PBParticipant, size),
        sizeof(((PBParticipant*)0)->size),
        offsetof(PBParticipant, written),
        sizeof(((PBParticipant*)0)->written),
        offsetof(PBParticipant, account_id),
        sizeof(((PBParticipant*)0)->account_id),
        offsetof(PBParticipant, device_id),
        sizeof(((PBParticipant*)0)->device_id),
        offsetof(PBParticipant, instance_id),
        sizeof(((PBParticipant*)0)->instance_id),
        offsetof(PBParticipant, build_id),
        sizeof(((PBParticipant*)0)->build_id),
        offsetof(PBParticipant, display_name),
        sizeof(((PBParticipant*)0)->display_name),
        sizeof(PBFileReference),
        _Alignof(PBFileReference),
        5,
        offsetof(PBFileReference, size),
        sizeof(((PBFileReference*)0)->size),
        offsetof(PBFileReference, written),
        sizeof(((PBFileReference*)0)->written),
        offsetof(PBFileReference, volume),
        sizeof(((PBFileReference*)0)->volume),
        offsetof(PBFileReference, entry_id),
        sizeof(((PBFileReference*)0)->entry_id),
        offsetof(PBFileReference, reserved),
        sizeof(((PBFileReference*)0)->reserved),
        sizeof(PBFileStatus),
        _Alignof(PBFileStatus),
        6,
        offsetof(PBFileStatus, size),
        sizeof(((PBFileStatus*)0)->size),
        offsetof(PBFileStatus, written),
        sizeof(((PBFileStatus*)0)->written),
        offsetof(PBFileStatus, kind),
        sizeof(((PBFileStatus*)0)->kind),
        offsetof(PBFileStatus, flags),
        sizeof(((PBFileStatus*)0)->flags),
        offsetof(PBFileStatus, file_size),
        sizeof(((PBFileStatus*)0)->file_size),
        offsetof(PBFileStatus, modification_time),
        sizeof(((PBFileStatus*)0)->modification_time),
        sizeof(PBFileCloseResult),
        _Alignof(PBFileCloseResult),
        5,
        offsetof(PBFileCloseResult, size),
        sizeof(((PBFileCloseResult*)0)->size),
        offsetof(PBFileCloseResult, written),
        sizeof(((PBFileCloseResult*)0)->written),
        offsetof(PBFileCloseResult, error),
        sizeof(((PBFileCloseResult*)0)->error),
        offsetof(PBFileCloseResult, publication),
        sizeof(((PBFileCloseResult*)0)->publication),
        offsetof(PBFileCloseResult, recovery),
        sizeof(((PBFileCloseResult*)0)->recovery),
        sizeof(PBFileListEntry),
        _Alignof(PBFileListEntry),
        7,
        offsetof(PBFileListEntry, size),
        sizeof(((PBFileListEntry*)0)->size),
        offsetof(PBFileListEntry, written),
        sizeof(((PBFileListEntry*)0)->written),
        offsetof(PBFileListEntry, outcome),
        sizeof(((PBFileListEntry*)0)->outcome),
        offsetof(PBFileListEntry, kind),
        sizeof(((PBFileListEntry*)0)->kind),
        offsetof(PBFileListEntry, name_data),
        sizeof(((PBFileListEntry*)0)->name_data),
        offsetof(PBFileListEntry, name_capacity),
        sizeof(((PBFileListEntry*)0)->name_capacity),
        offsetof(PBFileListEntry, name_count),
        sizeof(((PBFileListEntry*)0)->name_count),
        sizeof(PBSelection),
        _Alignof(PBSelection),
        7,
        offsetof(PBSelection, size),
        sizeof(((PBSelection*)0)->size),
        offsetof(PBSelection, written),
        sizeof(((PBSelection*)0)->written),
        offsetof(PBSelection, present),
        sizeof(((PBSelection*)0)->present),
        offsetof(PBSelection, reserved),
        sizeof(((PBSelection*)0)->reserved),
        offsetof(PBSelection, volume),
        sizeof(((PBSelection*)0)->volume),
        offsetof(PBSelection, entry_id),
        sizeof(((PBSelection*)0)->entry_id),
        offsetof(PBSelection, entry_reserved),
        sizeof(((PBSelection*)0)->entry_reserved),
        sizeof(PBEvent),
        _Alignof(PBEvent),
        6,
        offsetof(PBEvent, size),
        sizeof(((PBEvent*)0)->size),
        offsetof(PBEvent, written),
        sizeof(((PBEvent*)0)->written),
        offsetof(PBEvent, generation),
        sizeof(((PBEvent*)0)->generation),
        offsetof(PBEvent, kind),
        sizeof(((PBEvent*)0)->kind),
        offsetof(PBEvent, target),
        sizeof(((PBEvent*)0)->target),
        offsetof(PBEvent, object_id),
        sizeof(((PBEvent*)0)->object_id),
        sizeof(PBIdentity),
        _Alignof(PBIdentity),
        10,
        offsetof(PBIdentity, size),
        sizeof(((PBIdentity*)0)->size),
        offsetof(PBIdentity, written),
        sizeof(((PBIdentity*)0)->written),
        offsetof(PBIdentity, generation),
        sizeof(((PBIdentity*)0)->generation),
        offsetof(PBIdentity, account_id),
        sizeof(((PBIdentity*)0)->account_id),
        offsetof(PBIdentity, device_id),
        sizeof(((PBIdentity*)0)->device_id),
        offsetof(PBIdentity, instance_id),
        sizeof(((PBIdentity*)0)->instance_id),
        offsetof(PBIdentity, session_id),
        sizeof(((PBIdentity*)0)->session_id),
        offsetof(PBIdentity, flags),
        sizeof(((PBIdentity*)0)->flags),
        offsetof(PBIdentity, reserved),
        sizeof(((PBIdentity*)0)->reserved),
        offsetof(PBIdentity, display_name),
        sizeof(((PBIdentity*)0)->display_name),
        sizeof(PBApp),
        _Alignof(PBApp),
        6,
        offsetof(PBApp, size),
        sizeof(((PBApp*)0)->size),
        offsetof(PBApp, written),
        sizeof(((PBApp*)0)->written),
        offsetof(PBApp, init),
        sizeof(((PBApp*)0)->init),
        offsetof(PBApp, turn),
        sizeof(((PBApp*)0)->turn),
        offsetof(PBApp, destroy),
        sizeof(((PBApp*)0)->destroy),
        offsetof(PBApp, user_data),
        sizeof(((PBApp*)0)->user_data),
        sizeof(PBFieldMetadata),
        _Alignof(PBFieldMetadata),
        15,
        offsetof(PBFieldMetadata, size),
        sizeof(((PBFieldMetadata*)0)->size),
        offsetof(PBFieldMetadata, written),
        sizeof(((PBFieldMetadata*)0)->written),
        offsetof(PBFieldMetadata, volume_id),
        sizeof(((PBFieldMetadata*)0)->volume_id),
        offsetof(PBFieldMetadata, object_id),
        sizeof(((PBFieldMetadata*)0)->object_id),
        offsetof(PBFieldMetadata, type_id),
        sizeof(((PBFieldMetadata*)0)->type_id),
        offsetof(PBFieldMetadata, field_id),
        sizeof(((PBFieldMetadata*)0)->field_id),
        offsetof(PBFieldMetadata, revision),
        sizeof(((PBFieldMetadata*)0)->revision),
        offsetof(PBFieldMetadata, principal_id),
        sizeof(((PBFieldMetadata*)0)->principal_id),
        offsetof(PBFieldMetadata, device_id),
        sizeof(((PBFieldMetadata*)0)->device_id),
        offsetof(PBFieldMetadata, state),
        sizeof(((PBFieldMetadata*)0)->state),
        offsetof(PBFieldMetadata, provenance),
        sizeof(((PBFieldMetadata*)0)->provenance),
        offsetof(PBFieldMetadata, principal_kind),
        sizeof(((PBFieldMetadata*)0)->principal_kind),
        offsetof(PBFieldMetadata, reserved),
        sizeof(((PBFieldMetadata*)0)->reserved),
        offsetof(PBFieldMetadata, entry_id),
        sizeof(((PBFieldMetadata*)0)->entry_id),
        offsetof(PBFieldMetadata, entry_reserved),
        sizeof(((PBFieldMetadata*)0)->entry_reserved),
        sizeof(PBAuthenticatedField),
        _Alignof(PBAuthenticatedField),
        10,
        offsetof(PBAuthenticatedField, size),
        sizeof(((PBAuthenticatedField*)0)->size),
        offsetof(PBAuthenticatedField, written),
        sizeof(((PBAuthenticatedField*)0)->written),
        offsetof(PBAuthenticatedField, metadata),
        sizeof(((PBAuthenticatedField*)0)->metadata),
        offsetof(PBAuthenticatedField, values_data),
        sizeof(((PBAuthenticatedField*)0)->values_data),
        offsetof(PBAuthenticatedField, values_capacity),
        sizeof(((PBAuthenticatedField*)0)->values_capacity),
        offsetof(PBAuthenticatedField, values_count),
        sizeof(((PBAuthenticatedField*)0)->values_count),
        offsetof(PBAuthenticatedField, values_stride),
        sizeof(((PBAuthenticatedField*)0)->values_stride),
        offsetof(PBAuthenticatedField, bytes_data),
        sizeof(((PBAuthenticatedField*)0)->bytes_data),
        offsetof(PBAuthenticatedField, bytes_capacity),
        sizeof(((PBAuthenticatedField*)0)->bytes_capacity),
        offsetof(PBAuthenticatedField, bytes_count),
        sizeof(((PBAuthenticatedField*)0)->bytes_count),
        sizeof(PBKvQuery),
        _Alignof(PBKvQuery),
        7,
        offsetof(PBKvQuery, size),
        sizeof(((PBKvQuery*)0)->size),
        offsetof(PBKvQuery, written),
        sizeof(((PBKvQuery*)0)->written),
        offsetof(PBKvQuery, root_level),
        sizeof(((PBKvQuery*)0)->root_level),
        offsetof(PBKvQuery, root_digest),
        sizeof(((PBKvQuery*)0)->root_digest),
        offsetof(PBKvQuery, has_root),
        sizeof(((PBKvQuery*)0)->has_root),
        offsetof(PBKvQuery, limit),
        sizeof(((PBKvQuery*)0)->limit),
        offsetof(PBKvQuery, reserved),
        sizeof(((PBKvQuery*)0)->reserved),
        sizeof(PBKvRead),
        _Alignof(PBKvRead),
        8,
        offsetof(PBKvRead, size),
        sizeof(((PBKvRead*)0)->size),
        offsetof(PBKvRead, written),
        sizeof(((PBKvRead*)0)->written),
        offsetof(PBKvRead, offset),
        sizeof(((PBKvRead*)0)->offset),
        offsetof(PBKvRead, limit),
        sizeof(((PBKvRead*)0)->limit),
        offsetof(PBKvRead, has_root),
        sizeof(((PBKvRead*)0)->has_root),
        offsetof(PBKvRead, root_level),
        sizeof(((PBKvRead*)0)->root_level),
        offsetof(PBKvRead, root_digest),
        sizeof(((PBKvRead*)0)->root_digest),
        offsetof(PBKvRead, reserved),
        sizeof(((PBKvRead*)0)->reserved),
        sizeof(PBBloblogBlob),
        _Alignof(PBBloblogBlob),
        4,
        offsetof(PBBloblogBlob, size),
        sizeof(((PBBloblogBlob*)0)->size),
        offsetof(PBBloblogBlob, written),
        sizeof(((PBBloblogBlob*)0)->written),
        offsetof(PBBloblogBlob, data),
        sizeof(((PBBloblogBlob*)0)->data),
        offsetof(PBBloblogBlob, length),
        sizeof(((PBBloblogBlob*)0)->length),
        sizeof(PBSessionInfo),
        _Alignof(PBSessionInfo),
        6,
        offsetof(PBSessionInfo, size),
        sizeof(((PBSessionInfo*)0)->size),
        offsetof(PBSessionInfo, written),
        sizeof(((PBSessionInfo*)0)->written),
        offsetof(PBSessionInfo, flags),
        sizeof(((PBSessionInfo*)0)->flags),
        offsetof(PBSessionInfo, reserved),
        sizeof(((PBSessionInfo*)0)->reserved),
        offsetof(PBSessionInfo, internal_id),
        sizeof(((PBSessionInfo*)0)->internal_id),
        offsetof(PBSessionInfo, key_secret),
        sizeof(((PBSessionInfo*)0)->key_secret),
    };
    return index < sizeof(layout) / sizeof(layout[0]) ? layout[index] : UINT32_MAX;
}
uint32_t probe_imports(void) {
    PBSessionInfo info = {.size = sizeof(PBSessionInfo), .written = 64};
    char code[PB_SESSION_CODE_CHARS + 1];
    const uint8_t seed[] = "session-seed-v1!";
    const char expected[] = "FFUk4yL7S6oHXq7rrK6a48";
    if (pb_session_code(&info, code) != 0 || code[0] != '\0')
        return 0x163;
    info.flags = PB_SESSION_HAS_KEY_SECRET;
    if (pb_session_code(&info, code) != PB_SESSION_CODE_CHARS || code[PB_SESSION_CODE_CHARS] != '\0')
        return 0x163;
    for (uint32_t i = 0; i < PB_SESSION_CODE_CHARS; i++)
        if (code[i] != '1')
            return 0x163;
    for (uint32_t i = 0; i < sizeof(info.key_secret); i++)
        info.key_secret[i] = seed[i];
    if (pb_session_code(&info, code) != PB_SESSION_CODE_CHARS)
        return 0x163;
    for (uint32_t i = 0; i < sizeof(expected); i++)
        if (code[i] != expected[i])
            return 0x163;
    PBContext *ctx = pb_guest_context();
    PBVolumeHandle volume = pb_handle_pack(UINT32_C(0x7edcba98), UINT32_C(0x89abcdef));
    PBOperationHandle handle = volume;
    PBFileHandle file = volume;
    PBFileListHandle list = volume;
    PBEntryHandle *entry = (void *)(storage + 64);
    *entry = (PBEntryHandle){.size=sizeof(PBEntryHandle), .volume=volume};
    if (pb_log(ctx, (void *)(storage + 128), UINT32_C(0x80000000) + 2) != PB_UNSUPPORTED)
        return 1;
    if (pb_view_submit(ctx, (void *)(storage + 128), UINT32_C(0x80000000) + 2) != PB_UNSUPPORTED)
        return 2;
    if (pb_event_poll(ctx, (void *)(storage + 128), (void *)(storage + 256),
                      UINT32_C(0x80000000) + 3) != PB_UNSUPPORTED)
        return 3;
    if (pb_event_poll_deadline(ctx, (void *)(storage + 128), (void *)(storage + 256),
                              UINT32_C(0x80000000) + 3, UINT64_C(0x8877665544332200) + 4) != PB_UNSUPPORTED)
        return 114;
    if (pb_identity(ctx, (void *)(storage + 128)) != PB_UNSUPPORTED)
        return 4;
    if (pb_session_info(ctx, (void *)(storage + 128)) != PB_UNSUPPORTED)
        return 0x163;
    if (pb_wake_after(ctx, UINT32_C(0x80000000) + 1) != PB_UNSUPPORTED)
        return 5;
    if (pb_update_defer(ctx, UINT64_C(0x8877665544332200) + 1) != PB_UNSUPPORTED)
        return 6;
    if (pb_volumes(ctx, (void *)(storage + 128)) != PB_UNSUPPORTED)
        return 9;
    if (pb_volumes_observe(ctx, UINT64_C(0x8877665544332201)) != PB_UNSUPPORTED)
        return 64;
    if (pb_capabilities(ctx, (void *)(storage + 128)) != PB_UNSUPPORTED)
        return 14;
    if (pb_startup_arguments(ctx, (void *)(storage + 128)) != PB_UNSUPPORTED)
        return 15;
    if (pb_presence(ctx, (void *)(storage + 128), UINT32_C(0x80000002), UINT32_C(0x80000003), (void *)(storage + 640)) != PB_UNSUPPORTED)
        return 16;
    if (pb_file_read_begin(ctx, file, UINT64_C(0x8877665544332200) + 2,
                     UINT32_C(0x80000000) + 3) != INT64_C(0x7edcba9889abcdef))
        return 18;
    if (pb_file_read_take(ctx, handle, (void *)(storage + 512), UINT32_C(0x80000003), (void *)(storage + 640)) != PB_UNSUPPORTED)
        return 40;
    if (pb_file_write_begin(ctx, file, UINT64_C(0x8877665544332200) + 2, (void *)(storage + 256),
                      UINT32_C(0x80000000) + 3) != INT64_C(0x7edcba9889abcdef))
        return 19;
    if (pb_file_write_take(ctx, handle) != PB_UNSUPPORTED)
        return 41;
    if (pb_file_open_begin(ctx, file, (void *)(storage + 128), UINT32_C(0x80000000) + 2, UINT32_C(0x80000000) + 3) != INT64_C(0x7edcba9889abcdef)) return 80;
    if (pb_file_open_take(ctx, handle) != INT64_C(0x7edcba9889abcdef)) return 81;
    if (pb_file_status_begin(ctx, file) != INT64_C(0x7edcba9889abcdef)) return 82;
    if (pb_file_status_take(ctx, handle, (void *)(storage + 512)) != PB_UNSUPPORTED) return 83;
    if (pb_file_close_begin(ctx, file, UINT32_C(0x80000000) + 2) != INT64_C(0x7edcba9889abcdef)) return 84;
    if (pb_file_close_take(ctx, handle, (void *)(storage + 512)) != PB_UNSUPPORTED) return 85;
    if (pb_file_list_open_begin(ctx, file, UINT32_C(0x80000000) + 2) != INT64_C(0x7edcba9889abcdef)) return 86;
    if (pb_file_list_open_take(ctx, handle) != INT64_C(0x7edcba9889abcdef)) return 87;
    if (pb_file_list_next_begin(ctx, list) != INT64_C(0x7edcba9889abcdef)) return 88;
    if (pb_file_list_next_take(ctx, handle, (void *)(storage + 512)) != PB_UNSUPPORTED) return 89;
    if (pb_file_list_close(ctx, list) != PB_UNSUPPORTED) return 90;
    if (pb_operation_dispose(ctx, handle) != PB_UNSUPPORTED) return 91;
    if (pb_file_dispose(ctx, file) != PB_UNSUPPORTED) return 92;
    if (pb_object_create_begin(ctx, entry, (void *)(storage + 256), (void *)(storage + 384), UINT32_C(0x80000000) + 4, UINT32_C(0x80000005)) !=
        INT64_C(0x7edcba9889abcdef))
        return 20;
    if (pb_object_create_poll(ctx, handle, (void *)(storage + 256)) != PB_UNSUPPORTED)
        return 21;
    if (pb_file_copy_begin(ctx, (void *)(storage + 128), (void *)(storage + 256)) != INT64_C(0x7edcba9889abcdef))
        return 22;
    if (pb_file_copy_poll(ctx, handle, (void *)(storage + 256)) != PB_UNSUPPORTED)
        return 23;
    if (pb_file_select_begin(ctx) != INT64_C(0x7edcba9889abcdef))
        return 24;
    if (pb_file_select_poll(ctx, handle, (void *)(storage + 256)) != PB_UNSUPPORTED)
        return 25;
    if (pb_object_read_begin(ctx, entry, (void *)(storage + 256), (void *)(storage + 384), UINT32_C(0x80000000) + 4) != INT64_C(0x7edcba9889abcdef))
        return 30;
    if (pb_object_read_take(ctx, handle, (void *)(storage + 256)) != PB_UNSUPPORTED)
        return 31;
    if (pb_object_edit_begin(ctx, entry, (void *)(storage + 256), (void *)(storage + 384), UINT32_C(0x80000000) + 4, UINT32_C(0x80000005)) != INT64_C(0x7edcba9889abcdef))
        return 32;
    if (pb_object_edit_take(ctx, handle) != PB_UNSUPPORTED)
        return 33;
    if (pb_object_query_begin(ctx, entry, (void *)(storage + 256)) != INT64_C(0x7edcba9889abcdef))
        return 34;
    if (pb_object_query_take(ctx, handle, (void *)(storage + 256)) != PB_UNSUPPORTED)
        return 35;
    if (pb_schema_read_begin(ctx, entry) != INT64_C(0x7edcba9889abcdef))
        return 36;
    if (pb_schema_read_take(ctx, handle, (void *)(storage + 256)) != PB_UNSUPPORTED)
        return 37;
    if (pb_schema_establish_begin(ctx, entry, (void *)(storage + 256), UINT32_C(0x80000000) + 3, UINT32_C(0x80000004)) != INT64_C(0x7edcba9889abcdef))
        return 38;
    if (pb_schema_establish_take(ctx, handle) != PB_UNSUPPORTED)
        return 39;
    if (pb_object_field_read_authenticated_begin(ctx, entry, (void *)(storage + 256),
                                           (void *)(storage + 384)) != INT64_C(0x7edcba9889abcdef))
        return 26;
    if (pb_object_field_read_authenticated_take(ctx, handle, (void *)(storage + 512)) != PB_UNSUPPORTED) return 40;
    if (pb_host_platform(ctx) != PB_UNSUPPORTED)
        return 27;
    if (pb_host_limit(ctx, 0x80000001) != PB_UNSUPPORTED)
        return 28;
    if (pb_entry_resolve_begin(ctx, volume, (void *)(storage + 128),
                               UINT32_C(0x80000000) + 2) != INT64_C(0x7edcba9889abcdef))
        return 29;
    if (pb_entry_resolve_take(ctx, handle, (void *)(storage + 256)) != PB_UNSUPPORTED)
        return 42;
    if (pb_kv_keys_begin(ctx, entry, (void *)(storage + 256), (void *)(storage + 384), 0x80000004) != INT64_C(0x7edcba9889abcdef))
        return 43;
    if (pb_kv_keys_take(ctx, handle, (void *)(storage + 256), UINT32_C(0x80000003), (void *)(storage + 640)) != PB_UNSUPPORTED)
        return 44;
    if (pb_kv_read_begin(ctx, entry, (void *)(storage + 256), 0x80000003, (void *)(storage + 384)) != INT64_C(0x7edcba9889abcdef))
        return 45;
    if (pb_kv_read_take(ctx, handle, (void *)(storage + 256), UINT32_C(0x80000003), (void *)(storage + 640)) != PB_UNSUPPORTED)
        return 46;
    if (pb_kv_put_begin(ctx, entry, (void *)(storage + 128), 0x80000003, (void *)(storage + 256), 0x80000005) != INT64_C(0x7edcba9889abcdef))
        return 47;
    if (pb_kv_put_take(ctx, handle) != PB_UNSUPPORTED)
        return 48;
    if (pb_kv_delete_begin(ctx, entry, (void *)(storage + 128), 0x80000003) != INT64_C(0x7edcba9889abcdef))
        return 49;
    if (pb_kv_delete_take(ctx, handle) != PB_UNSUPPORTED)
        return 50;
    if (pb_trace_record(ctx, (void *)(storage + 128), 0x80000001, UINT64_C(0x8877665544332200) + 2, UINT64_C(0x8877665544332200) + 3) != PB_UNSUPPORTED)
        return 51;
    if (pb_clock_read(ctx) != UINT64_C(0x8877665544332201))
        return 52;
    const union { uint32_t bits; float value; } float1 = { .bits = UINT32_C(0x80000001) };
    const union { uint32_t bits; float value; } float2 = { .bits = UINT32_C(0x80000002) };
    const PBSoundInstance sound = { .sound = volume, .instance = UINT32_C(0x80000001) };
    if (pb_audio_open_begin(ctx, 0x80000001) != INT64_C(0x7edcba9889abcdef)) return 100;
    if (pb_audio_open_take(ctx, handle) != INT64_C(0x7edcba9889abcdef)) return 101;
    if (pb_audio_close(ctx, volume) != PB_UNSUPPORTED) return 102;
    if (pb_audio_sample_rate(ctx, volume) != PB_UNSUPPORTED) return 103;
    if (pb_audio_volume(ctx, volume) != PB_UNSUPPORTED) return 104;
    if (pb_audio_set_volume(ctx, volume, float1.value) != PB_UNSUPPORTED) return 105;
    if (pb_sound_open_pcm(ctx, volume, 0x80000001, 0x80000002, (void *)(storage + 128), 0x80000003) != INT64_C(0x7edcba9889abcdef)) return 106;
    if (pb_sound_close(ctx, volume) != PB_UNSUPPORTED) return 107;
    if (pb_sound_play(ctx, sound, 0x80000002, UINT64_C(0x8877665544332201), UINT64_C(0x8877665544332202), 0x80000003) != PB_UNSUPPORTED) return 108;
    if (pb_sound_tell(ctx, sound, (void *)(storage + 512)) != PB_UNSUPPORTED) return 109;
    if (pb_sound_seek(ctx, sound, UINT64_C(0x8877665544332202)) != PB_UNSUPPORTED) return 110;
    if (pb_sound_set_gain(ctx, sound, 0x80000002) != PB_UNSUPPORTED) return 111;
    if (pb_sound_set_balance(ctx, sound, float2.value) != PB_UNSUPPORTED) return 112;
    if (pb_sound_stop(ctx, sound) != PB_UNSUPPORTED) return 113;
    return 0;
}
uint32_t probe_invalid(void) {
    PBVolumes output = PB_RECORD_INIT(PBVolumes);
    PBEntryHandle entry = {.size=sizeof(PBEntryHandle), .volume=UINT64_C(0x100000001)};
    if (pb_host_platform(NULL) != PB_INVALID_ARGUMENT)
        return 3;
    if (pb_volumes(NULL, &output) != PB_INVALID_ARGUMENT)
        return 1;
    if (pb_object_query_begin(NULL, &entry, NULL) != PB_INVALID_ARGUMENT)
        return 2;
    return 0;
}
