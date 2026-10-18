//! Every catalog view the pinned build reports, and what the capture does
//! with it (#2153). A view the capture does not read whole is here with its
//! reason, so the list is a decision, not an omission: the coverage test
//! (`mssql_every_catalog_view_is_read_or_set_aside_with_a_reason`) fails on
//! a view a later build adds until it is classed here.

/// What the capture does with one catalog view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Read whole, its rows filed under what they describe (`columns::VIEWS`).
    Read,
    /// Read as the definition an id names: a data space, an XML schema
    /// collection, a column encryption key, a full-text catalog or stoplist.
    Named,
    /// Read by the typed reads the capture's logic uses.
    Typed,
    /// What it describes refuses by name when the capture meets it.
    Refused(&'static str),
    /// Not read, for the reason given.
    Skipped(&'static str),
}

const COMPATIBILITY: &str = "a backward-compatibility view over catalogs read directly";
const ENGINE: &str =
    "the engine's own objects, or every object with them: the capture reads the database's own";
const INSTANCE: &str =
    "instance scope or a fixed engine list: nothing a database's objects declare";
const TELEMETRY: &str = "runtime state or telemetry, not a declaration";
const BROKER: &str = "Service Broker, which no managed object binds";
const AUTHORIZATION: &str =
    "authorization, which the deployment authorization context reads (#611)";
const DATABASE: &str = "database-level configuration, which the analysis scope qualifies (#611)";
const DERIVED: &str = "derived from catalogs read directly";
const UNBOUND: &str =
    "binds nothing a creation reads: documentation, labels, statistics or plan advice";
const CLR: &str = "CLR: an assembly's code is not read, so a CLR object or type refuses by name";
const EXTERNAL: &str =
    "an external object's shape is not read, so an external table refuses by name";
const NUMBERED: &str =
    "a numbered procedure group is not read, so a procedure that has one refuses by name";

/// Every catalog view under `sys` on the pinned build, the dynamic
/// management views (`dm_*`) aside: they are runtime state, not a catalog.
pub const CATALOG: &[(&str, Disposition)] = &[
    ("all_columns", Disposition::Skipped(ENGINE)),
    ("all_objects", Disposition::Skipped(ENGINE)),
    ("all_parameters", Disposition::Skipped(ENGINE)),
    ("all_sql_modules", Disposition::Skipped(ENGINE)),
    ("all_views", Disposition::Skipped(ENGINE)),
    ("allocation_units", Disposition::Skipped(TELEMETRY)),
    ("assemblies", Disposition::Refused(CLR)),
    ("assembly_files", Disposition::Refused(CLR)),
    ("assembly_modules", Disposition::Refused(CLR)),
    ("assembly_references", Disposition::Refused(CLR)),
    ("assembly_types", Disposition::Refused(CLR)),
    ("asymmetric_keys", Disposition::Skipped(AUTHORIZATION)),
    (
        "availability_databases_cluster",
        Disposition::Skipped(INSTANCE),
    ),
    (
        "availability_group_listener_ip_addresses",
        Disposition::Skipped(INSTANCE),
    ),
    (
        "availability_group_listeners",
        Disposition::Skipped(INSTANCE),
    ),
    ("availability_groups", Disposition::Skipped(INSTANCE)),
    (
        "availability_groups_cluster",
        Disposition::Skipped(INSTANCE),
    ),
    (
        "availability_read_only_routing_lists",
        Disposition::Skipped(INSTANCE),
    ),
    ("availability_replicas", Disposition::Skipped(INSTANCE)),
    ("backup_devices", Disposition::Skipped(INSTANCE)),
    ("certificates", Disposition::Skipped(AUTHORIZATION)),
    ("change_tracking_databases", Disposition::Skipped(DATABASE)),
    ("change_tracking_tables", Disposition::Read),
    ("check_constraints", Disposition::Read),
    ("column_encryption_key_values", Disposition::Named),
    ("column_encryption_keys", Disposition::Named),
    ("column_master_keys", Disposition::Named),
    ("column_store_dictionaries", Disposition::Skipped(TELEMETRY)),
    ("column_store_row_groups", Disposition::Skipped(TELEMETRY)),
    ("column_store_segments", Disposition::Skipped(TELEMETRY)),
    ("column_type_usages", Disposition::Skipped(DERIVED)),
    (
        "column_xml_schema_collection_usages",
        Disposition::Skipped(DERIVED),
    ),
    ("columns", Disposition::Read),
    ("computed_columns", Disposition::Read),
    ("configurations", Disposition::Skipped(INSTANCE)),
    ("conversation_endpoints", Disposition::Skipped(TELEMETRY)),
    ("conversation_groups", Disposition::Skipped(TELEMETRY)),
    ("conversation_priorities", Disposition::Skipped(BROKER)),
    ("credentials", Disposition::Skipped(INSTANCE)),
    ("crypt_properties", Disposition::Skipped(AUTHORIZATION)),
    ("cryptographic_providers", Disposition::Skipped(INSTANCE)),
    ("data_spaces", Disposition::Named),
    (
        "database_audit_specification_details",
        Disposition::Skipped(AUTHORIZATION),
    ),
    (
        "database_audit_specifications",
        Disposition::Skipped(AUTHORIZATION),
    ),
    (
        "database_automatic_tuning_configurations",
        Disposition::Skipped(TELEMETRY),
    ),
    (
        "database_automatic_tuning_mode",
        Disposition::Skipped(TELEMETRY),
    ),
    (
        "database_automatic_tuning_options",
        Disposition::Skipped(TELEMETRY),
    ),
    ("database_credentials", Disposition::Skipped(AUTHORIZATION)),
    ("database_files", Disposition::Skipped(DATABASE)),
    (
        "database_filestream_options",
        Disposition::Skipped(DATABASE),
    ),
    ("database_ledger_blocks", Disposition::Skipped(TELEMETRY)),
    (
        "database_ledger_digest_locations",
        Disposition::Skipped(TELEMETRY),
    ),
    (
        "database_ledger_transactions",
        Disposition::Skipped(TELEMETRY),
    ),
    ("database_mirroring", Disposition::Skipped(INSTANCE)),
    (
        "database_mirroring_endpoints",
        Disposition::Skipped(INSTANCE),
    ),
    (
        "database_mirroring_witnesses",
        Disposition::Skipped(INSTANCE),
    ),
    ("database_permissions", Disposition::Skipped(AUTHORIZATION)),
    ("database_principals", Disposition::Typed),
    (
        "database_query_store_internal_state",
        Disposition::Skipped(TELEMETRY),
    ),
    (
        "database_query_store_options",
        Disposition::Skipped(TELEMETRY),
    ),
    ("database_recovery_status", Disposition::Skipped(TELEMETRY)),
    ("database_role_members", Disposition::Skipped(AUTHORIZATION)),
    (
        "database_scoped_configurations",
        Disposition::Skipped(DATABASE),
    ),
    (
        "database_scoped_credentials",
        Disposition::Skipped(AUTHORIZATION),
    ),
    ("databases", Disposition::Skipped(INSTANCE)),
    ("default_constraints", Disposition::Read),
    ("destination_data_spaces", Disposition::Named),
    ("edge_constraint_clauses", Disposition::Read),
    ("edge_constraints", Disposition::Read),
    ("endpoint_webmethods", Disposition::Skipped(INSTANCE)),
    ("endpoints", Disposition::Skipped(INSTANCE)),
    (
        "event_notification_event_types",
        Disposition::Skipped(INSTANCE),
    ),
    ("event_notifications", Disposition::Skipped(UNBOUND)),
    ("events", Disposition::Skipped(INSTANCE)),
    ("extended_procedures", Disposition::Refused(CLR)),
    ("extended_properties", Disposition::Skipped(UNBOUND)),
    ("external_data_sources", Disposition::Refused(EXTERNAL)),
    ("external_file_formats", Disposition::Refused(EXTERNAL)),
    (
        "external_governance_classification_attributes",
        Disposition::Refused(EXTERNAL),
    ),
    (
        "external_governance_classifications",
        Disposition::Refused(EXTERNAL),
    ),
    (
        "external_governance_classifications_mapping",
        Disposition::Refused(EXTERNAL),
    ),
    (
        "external_governance_sensitivity_classifications",
        Disposition::Refused(EXTERNAL),
    ),
    (
        "external_governance_sensitivity_labels",
        Disposition::Refused(EXTERNAL),
    ),
    (
        "external_governance_sensitivity_labels_mapping",
        Disposition::Refused(EXTERNAL),
    ),
    ("external_job_streams", Disposition::Refused(EXTERNAL)),
    ("external_language_files", Disposition::Refused(EXTERNAL)),
    ("external_languages", Disposition::Refused(EXTERNAL)),
    ("external_libraries", Disposition::Refused(EXTERNAL)),
    (
        "external_libraries_installed",
        Disposition::Refused(EXTERNAL),
    ),
    ("external_library_files", Disposition::Refused(EXTERNAL)),
    (
        "external_library_setup_errors",
        Disposition::Refused(EXTERNAL),
    ),
    ("external_models", Disposition::Refused(EXTERNAL)),
    ("external_stream_columns", Disposition::Refused(EXTERNAL)),
    ("external_streaming_jobs", Disposition::Refused(EXTERNAL)),
    ("external_streams", Disposition::Refused(EXTERNAL)),
    ("external_table_columns", Disposition::Refused(EXTERNAL)),
    (
        "external_table_partitioning_columns",
        Disposition::Refused(EXTERNAL),
    ),
    (
        "external_table_schema_changed_mdsync",
        Disposition::Refused(EXTERNAL),
    ),
    ("external_tables", Disposition::Refused(EXTERNAL)),
    ("filegroups", Disposition::Named),
    (
        "filetable_system_defined_objects",
        Disposition::Skipped(DERIVED),
    ),
    ("filetables", Disposition::Read),
    ("foreign_key_columns", Disposition::Read),
    ("foreign_keys", Disposition::Read),
    ("fulltext_catalogs", Disposition::Named),
    ("fulltext_document_types", Disposition::Skipped(INSTANCE)),
    (
        "fulltext_index_catalog_usages",
        Disposition::Skipped(DERIVED),
    ),
    ("fulltext_index_columns", Disposition::Read),
    ("fulltext_index_fragments", Disposition::Skipped(TELEMETRY)),
    ("fulltext_indexes", Disposition::Read),
    ("fulltext_languages", Disposition::Skipped(INSTANCE)),
    (
        "fulltext_semantic_language_statistics_database",
        Disposition::Skipped(DATABASE),
    ),
    (
        "fulltext_semantic_languages",
        Disposition::Skipped(INSTANCE),
    ),
    ("fulltext_stoplists", Disposition::Named),
    ("fulltext_stopwords", Disposition::Skipped(UNBOUND)),
    ("fulltext_system_stopwords", Disposition::Skipped(INSTANCE)),
    ("function_order_columns", Disposition::Refused(CLR)),
    ("hash_indexes", Disposition::Read),
    ("http_endpoints", Disposition::Skipped(INSTANCE)),
    ("identity_columns", Disposition::Read),
    ("index_columns", Disposition::Read),
    (
        "index_resumable_operations",
        Disposition::Skipped(TELEMETRY),
    ),
    ("indexes", Disposition::Read),
    (
        "information_protection_label_mapping",
        Disposition::Skipped(UNBOUND),
    ),
    ("internal_partitions", Disposition::Skipped(TELEMETRY)),
    ("internal_tables", Disposition::Skipped(DERIVED)),
    ("json_index_paths", Disposition::Read),
    ("json_indexes", Disposition::Read),
    ("key_constraints", Disposition::Read),
    ("key_encryptions", Disposition::Skipped(AUTHORIZATION)),
    ("ledger_column_history", Disposition::Skipped(TELEMETRY)),
    ("ledger_table_history", Disposition::Skipped(TELEMETRY)),
    ("linked_logins", Disposition::Skipped(INSTANCE)),
    ("login_token", Disposition::Skipped(INSTANCE)),
    ("masked_columns", Disposition::Read),
    ("master_files", Disposition::Skipped(INSTANCE)),
    ("master_key_passwords", Disposition::Skipped(AUTHORIZATION)),
    (
        "memory_optimized_tables_internal_attributes",
        Disposition::Skipped(DERIVED),
    ),
    (
        "message_type_xml_schema_collection_usages",
        Disposition::Skipped(BROKER),
    ),
    ("messages", Disposition::Skipped(INSTANCE)),
    ("module_assembly_usages", Disposition::Refused(CLR)),
    (
        "numbered_procedure_parameters",
        Disposition::Refused(NUMBERED),
    ),
    ("numbered_procedures", Disposition::Refused(NUMBERED)),
    ("objects", Disposition::Read),
    ("openkeys", Disposition::Skipped(TELEMETRY)),
    ("parameter_type_usages", Disposition::Skipped(DERIVED)),
    (
        "parameter_xml_schema_collection_usages",
        Disposition::Skipped(DERIVED),
    ),
    ("parameters", Disposition::Read),
    ("partition_functions", Disposition::Named),
    ("partition_parameters", Disposition::Named),
    ("partition_range_values", Disposition::Named),
    ("partition_schemes", Disposition::Named),
    ("partitions", Disposition::Read),
    ("periods", Disposition::Read),
    ("plan_guides", Disposition::Skipped(UNBOUND)),
    ("procedures", Disposition::Read),
    ("query_context_settings", Disposition::Skipped(TELEMETRY)),
    ("query_store_plan", Disposition::Skipped(TELEMETRY)),
    ("query_store_plan_feedback", Disposition::Skipped(TELEMETRY)),
    (
        "query_store_plan_forcing_locations",
        Disposition::Skipped(TELEMETRY),
    ),
    ("query_store_query", Disposition::Skipped(TELEMETRY)),
    ("query_store_query_hints", Disposition::Skipped(TELEMETRY)),
    ("query_store_query_text", Disposition::Skipped(TELEMETRY)),
    ("query_store_query_variant", Disposition::Skipped(TELEMETRY)),
    ("query_store_replicas", Disposition::Skipped(TELEMETRY)),
    ("query_store_runtime_stats", Disposition::Skipped(TELEMETRY)),
    (
        "query_store_runtime_stats_interval",
        Disposition::Skipped(TELEMETRY),
    ),
    ("query_store_wait_stats", Disposition::Skipped(TELEMETRY)),
    (
        "registered_search_properties",
        Disposition::Skipped(INSTANCE),
    ),
    ("registered_search_property_lists", Disposition::Named),
    (
        "remote_data_archive_databases",
        Disposition::Skipped(DATABASE),
    ),
    ("remote_data_archive_tables", Disposition::Skipped(DERIVED)),
    ("remote_logins", Disposition::Skipped(INSTANCE)),
    ("remote_service_bindings", Disposition::Skipped(BROKER)),
    (
        "resource_governor_configuration",
        Disposition::Skipped(INSTANCE),
    ),
    (
        "resource_governor_external_resource_pool_affinity",
        Disposition::Skipped(INSTANCE),
    ),
    (
        "resource_governor_external_resource_pools",
        Disposition::Skipped(INSTANCE),
    ),
    (
        "resource_governor_resource_pool_affinity",
        Disposition::Skipped(INSTANCE),
    ),
    (
        "resource_governor_resource_pools",
        Disposition::Skipped(INSTANCE),
    ),
    (
        "resource_governor_workload_groups",
        Disposition::Skipped(INSTANCE),
    ),
    ("routes", Disposition::Skipped(BROKER)),
    ("schemas", Disposition::Typed),
    ("securable_classes", Disposition::Skipped(INSTANCE)),
    ("security_policies", Disposition::Read),
    ("security_predicates", Disposition::Read),
    ("selective_xml_index_namespaces", Disposition::Read),
    ("selective_xml_index_paths", Disposition::Read),
    ("sensitivity_classifications", Disposition::Skipped(UNBOUND)),
    ("sequences", Disposition::Read),
    ("server_assembly_modules", Disposition::Skipped(INSTANCE)),
    (
        "server_audit_specification_details",
        Disposition::Skipped(INSTANCE),
    ),
    (
        "server_audit_specifications",
        Disposition::Skipped(INSTANCE),
    ),
    ("server_audits", Disposition::Skipped(INSTANCE)),
    ("server_event_notifications", Disposition::Skipped(INSTANCE)),
    (
        "server_event_session_actions",
        Disposition::Skipped(INSTANCE),
    ),
    (
        "server_event_session_events",
        Disposition::Skipped(INSTANCE),
    ),
    (
        "server_event_session_fields",
        Disposition::Skipped(INSTANCE),
    ),
    (
        "server_event_session_targets",
        Disposition::Skipped(INSTANCE),
    ),
    ("server_event_sessions", Disposition::Skipped(INSTANCE)),
    ("server_events", Disposition::Skipped(INSTANCE)),
    ("server_file_audits", Disposition::Skipped(INSTANCE)),
    (
        "server_memory_optimized_hybrid_buffer_pool_configuration",
        Disposition::Skipped(INSTANCE),
    ),
    ("server_permissions", Disposition::Skipped(INSTANCE)),
    (
        "server_principal_credentials",
        Disposition::Skipped(INSTANCE),
    ),
    ("server_principals", Disposition::Skipped(INSTANCE)),
    ("server_role_members", Disposition::Skipped(INSTANCE)),
    ("server_sql_modules", Disposition::Skipped(INSTANCE)),
    ("server_trigger_events", Disposition::Skipped(INSTANCE)),
    ("server_triggers", Disposition::Skipped(INSTANCE)),
    ("servers", Disposition::Skipped(INSTANCE)),
    ("service_broker_endpoints", Disposition::Skipped(INSTANCE)),
    (
        "service_contract_message_usages",
        Disposition::Skipped(BROKER),
    ),
    ("service_contract_usages", Disposition::Skipped(BROKER)),
    ("service_contracts", Disposition::Skipped(BROKER)),
    ("service_message_types", Disposition::Skipped(BROKER)),
    ("service_queue_usages", Disposition::Skipped(BROKER)),
    ("service_queues", Disposition::Skipped(BROKER)),
    ("services", Disposition::Skipped(BROKER)),
    ("soap_endpoints", Disposition::Skipped(INSTANCE)),
    ("spatial_index_tessellations", Disposition::Read),
    ("spatial_indexes", Disposition::Read),
    ("spatial_reference_systems", Disposition::Skipped(INSTANCE)),
    ("sql_dependencies", Disposition::Skipped(DERIVED)),
    ("sql_expression_dependencies", Disposition::Typed),
    ("sql_logins", Disposition::Skipped(INSTANCE)),
    ("sql_modules", Disposition::Read),
    ("stats", Disposition::Skipped(UNBOUND)),
    ("stats_columns", Disposition::Skipped(UNBOUND)),
    ("symmetric_keys", Disposition::Skipped(AUTHORIZATION)),
    ("synonyms", Disposition::Read),
    ("sysaltfiles", Disposition::Skipped(COMPATIBILITY)),
    ("syscacheobjects", Disposition::Skipped(COMPATIBILITY)),
    ("syscharsets", Disposition::Skipped(COMPATIBILITY)),
    ("syscolumns", Disposition::Skipped(COMPATIBILITY)),
    ("syscomments", Disposition::Skipped(COMPATIBILITY)),
    ("sysconfigures", Disposition::Skipped(COMPATIBILITY)),
    ("sysconstraints", Disposition::Skipped(COMPATIBILITY)),
    ("syscscontainers", Disposition::Skipped(COMPATIBILITY)),
    ("syscurconfigs", Disposition::Skipped(COMPATIBILITY)),
    ("syscursorcolumns", Disposition::Skipped(COMPATIBILITY)),
    ("syscursorrefs", Disposition::Skipped(COMPATIBILITY)),
    ("syscursors", Disposition::Skipped(COMPATIBILITY)),
    ("syscursortables", Disposition::Skipped(COMPATIBILITY)),
    ("sysdatabases", Disposition::Skipped(COMPATIBILITY)),
    ("sysdepends", Disposition::Skipped(COMPATIBILITY)),
    ("sysdevices", Disposition::Skipped(COMPATIBILITY)),
    ("sysfilegroups", Disposition::Skipped(COMPATIBILITY)),
    ("sysfiles", Disposition::Skipped(COMPATIBILITY)),
    ("sysforeignkeys", Disposition::Skipped(COMPATIBILITY)),
    ("sysfulltextcatalogs", Disposition::Skipped(COMPATIBILITY)),
    ("sysindexes", Disposition::Skipped(COMPATIBILITY)),
    ("sysindexkeys", Disposition::Skipped(COMPATIBILITY)),
    ("syslanguages", Disposition::Skipped(COMPATIBILITY)),
    ("syslockinfo", Disposition::Skipped(COMPATIBILITY)),
    ("syslogins", Disposition::Skipped(COMPATIBILITY)),
    ("sysmembers", Disposition::Skipped(COMPATIBILITY)),
    ("sysmessages", Disposition::Skipped(COMPATIBILITY)),
    ("sysobjects", Disposition::Skipped(COMPATIBILITY)),
    ("sysoledbusers", Disposition::Skipped(COMPATIBILITY)),
    ("sysopentapes", Disposition::Skipped(COMPATIBILITY)),
    ("sysperfinfo", Disposition::Skipped(COMPATIBILITY)),
    ("syspermissions", Disposition::Skipped(COMPATIBILITY)),
    ("sysprocesses", Disposition::Skipped(COMPATIBILITY)),
    ("sysprotects", Disposition::Skipped(COMPATIBILITY)),
    ("sysreferences", Disposition::Skipped(COMPATIBILITY)),
    ("sysremotelogins", Disposition::Skipped(COMPATIBILITY)),
    ("sysservers", Disposition::Skipped(COMPATIBILITY)),
    ("system_columns", Disposition::Skipped(ENGINE)),
    (
        "system_components_surface_area_configuration",
        Disposition::Skipped(ENGINE),
    ),
    (
        "system_internals_allocation_units",
        Disposition::Skipped(ENGINE),
    ),
    (
        "system_internals_partition_columns",
        Disposition::Skipped(ENGINE),
    ),
    ("system_internals_partitions", Disposition::Skipped(ENGINE)),
    ("system_objects", Disposition::Skipped(ENGINE)),
    ("system_parameters", Disposition::Skipped(ENGINE)),
    ("system_sql_modules", Disposition::Skipped(ENGINE)),
    ("system_views", Disposition::Skipped(ENGINE)),
    ("systypes", Disposition::Skipped(COMPATIBILITY)),
    ("sysusers", Disposition::Skipped(COMPATIBILITY)),
    ("table_types", Disposition::Read),
    ("tables", Disposition::Read),
    ("tcp_endpoints", Disposition::Skipped(INSTANCE)),
    ("time_zone_info", Disposition::Skipped(INSTANCE)),
    ("trace_categories", Disposition::Skipped(INSTANCE)),
    ("trace_columns", Disposition::Skipped(INSTANCE)),
    ("trace_event_bindings", Disposition::Skipped(INSTANCE)),
    ("trace_events", Disposition::Skipped(INSTANCE)),
    ("trace_subclass_values", Disposition::Skipped(INSTANCE)),
    ("traces", Disposition::Skipped(INSTANCE)),
    ("transmission_queue", Disposition::Skipped(TELEMETRY)),
    ("trigger_event_types", Disposition::Skipped(INSTANCE)),
    ("trigger_events", Disposition::Read),
    ("triggers", Disposition::Read),
    ("trusted_assemblies", Disposition::Refused(CLR)),
    ("type_assembly_usages", Disposition::Refused(CLR)),
    ("types", Disposition::Read),
    ("user_token", Disposition::Skipped(TELEMETRY)),
    ("vector_indexes", Disposition::Read),
    ("via_endpoints", Disposition::Skipped(INSTANCE)),
    ("views", Disposition::Read),
    ("xml_indexes", Disposition::Read),
    ("xml_schema_attributes", Disposition::Named),
    ("xml_schema_collections", Disposition::Named),
    ("xml_schema_component_placements", Disposition::Named),
    ("xml_schema_components", Disposition::Named),
    ("xml_schema_elements", Disposition::Named),
    ("xml_schema_facets", Disposition::Named),
    ("xml_schema_model_groups", Disposition::Named),
    ("xml_schema_namespaces", Disposition::Named),
    ("xml_schema_types", Disposition::Named),
    ("xml_schema_wildcard_namespaces", Disposition::Named),
    ("xml_schema_wildcards", Disposition::Named),
];

/// The disposition of `sys.<view>`, if the capture has one.
pub fn disposition(view: &str) -> Option<Disposition> {
    CATALOG
        .iter()
        .find(|(name, _)| *name == view)
        .map(|(_, disposition)| *disposition)
}
