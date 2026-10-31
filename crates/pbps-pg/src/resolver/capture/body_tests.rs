//! Stored function bodies (`pg_proc.prosqlbody`) through the input-path decoder
//! (#2563). Every tree is the text the engine stored on PostgreSQL 16.15 and
//! 18.6 for the function named in its constant:
//!
//! ```sql
//! f1  (x int)  LANGUAGE sql IMMUTABLE RETURN x + 1
//! f2  (x int)  LANGUAGE sql IMMUTABLE BEGIN ATOMIC SELECT x * 2; END
//! f4  (x text) LANGUAGE sql IMMUTABLE RETURN x LIKE '%@%' AND app.f1(1) = 2
//! two (x int)  LANGUAGE sql IMMUTABLE BEGIN ATOMIC SELECT x; SELECT x + 1; END
//! e1  ()       RETURNS void LANGUAGE sql BEGIN ATOMIC END
//! sel (x int)  ... BEGIN ATOMIC SELECT count(*)::int FROM pg_class WHERE oid = x; END
//! sub (x int)  ... BEGIN ATOMIC SELECT (SELECT 1) + x; END
//! srf (x int)  RETURNS SETOF int ... BEGIN ATOMIC SELECT generate_series(1, x); END
//! ```

use super::{BodyError, bound_body, bound_objects};
use std::collections::BTreeSet;

pub(crate) const RETURN_BODY_16: &str = r##"{QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :isReturn true :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeUseOuterJoin false :targetList ({TARGETENTRY :expr {OPEXPR :opno 551 :opfuncid 177 :opresulttype 23 :opretset false :opcollid 0 :inputcollid 0 :args ({PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location 72} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 76 :constvalue 4 [ 1 0 0 0 0 0 0 0 ]}) :location 74} :resno 1 :resname <> :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location 0 :stmt_len 0}"##;

pub(crate) const ATOMIC_BODY_16: &str = r##"(({QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :isReturn false :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeUseOuterJoin false :targetList ({TARGETENTRY :expr {OPEXPR :opno 514 :opfuncid 141 :opresulttype 23 :opretset false :opcollid 0 :inputcollid 0 :args ({PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location 85} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 89 :constvalue 4 [ 2 0 0 0 0 0 0 0 ]}) :location 87} :resno 1 :resname ?column? :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location 0 :stmt_len 0}))"##;

pub(crate) const EMPTY_BODY_16: &str = r##"(<>)"##;

pub(crate) const CALLS_USER_BODY_16: &str = r##"{QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :isReturn true :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeUseOuterJoin false :targetList ({TARGETENTRY :expr {BOOLEXPR :boolop and :args ({OPEXPR :opno 1209 :opfuncid 850 :opresulttype 16 :opretset false :opcollid 0 :inputcollid 100 :args ({PARAM :paramkind 0 :paramid 1 :paramtype 25 :paramtypmod -1 :paramcollid 100 :location 77} {CONST :consttype 25 :consttypmod -1 :constcollid 100 :constlen -1 :constbyval false :constisnull false :location 84 :constvalue 7 [ 28 0 0 0 37 64 37 ]}) :location 79} {OPEXPR :opno 96 :opfuncid 65 :opresulttype 16 :opretset false :opcollid 0 :inputcollid 0 :args ({FUNCEXPR :funcid 16386 :funcresulttype 23 :funcretset false :funcvariadic false :funcformat 0 :funccollid 0 :inputcollid 0 :args ({CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 101 :constvalue 4 [ 1 0 0 0 0 0 0 0 ]}) :location 94} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 106 :constvalue 4 [ 2 0 0 0 0 0 0 0 ]}) :location 104}) :location 90} :resno 1 :resname <> :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location 0 :stmt_len 0}"##;

pub(crate) const TWO_STATEMENTS_16: &str = r##"(({QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :isReturn false :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeUseOuterJoin false :targetList ({TARGETENTRY :expr {PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location 97} :resno 1 :resname x :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location 0 :stmt_len 0} {QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :isReturn false :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeUseOuterJoin false :targetList ({TARGETENTRY :expr {OPEXPR :opno 551 :opfuncid 177 :opresulttype 23 :opretset false :opcollid 0 :inputcollid 0 :args ({PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location 107} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 111 :constvalue 4 [ 1 0 0 0 0 0 0 0 ]}) :location 109} :resno 1 :resname ?column? :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location 0 :stmt_len 0}))"##;

pub(crate) const READS_TABLE_16: &str = r##"(({QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs true :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :isReturn false :cteList <> :rtable ({RANGETBLENTRY :alias <> :eref {ALIAS :aliasname pg_class :colnames ("oid" "relname" "relnamespace" "reltype" "reloftype" "relowner" "relam" "relfilenode" "reltablespace" "relpages" "reltuples" "relallvisible" "reltoastrelid" "relhasindex" "relisshared" "relpersistence" "relkind" "relnatts" "relchecks" "relhasrules" "relhastriggers" "relhassubclass" "relrowsecurity" "relforcerowsecurity" "relispopulated" "relreplident" "relispartition" "relrewrite" "relfrozenxid" "relminmxid" "relacl" "reloptions" "relpartbound")} :rtekind 0 :relid 1259 :relkind r :rellockmode 1 :tablesample <> :perminfoindex 1 :lateral false :inh true :inFromCl true :securityQuals <>}) :rteperminfos ({RTEPERMISSIONINFO :relid 1259 :inh true :requiredPerms 2 :checkAsUser 0 :selectedCols (b 8) :insertedCols (b) :updatedCols (b)}) :jointree {FROMEXPR :fromlist ({RANGETBLREF :rtindex 1}) :quals {OPEXPR :opno 607 :opfuncid 184 :opresulttype 16 :opretset false :opcollid 0 :inputcollid 0 :args ({VAR :varno 1 :varattno 1 :vartype 26 :vartypmod -1 :varcollid 0 :varnullingrels (b) :varlevelsup 0 :varnosyn 1 :varattnosyn 1 :location 131} {RELABELTYPE :arg {PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location 137} :resulttype 26 :resulttypmod -1 :resultcollid 0 :relabelformat 2 :location -1}) :location 135}} :mergeActionList <> :mergeUseOuterJoin false :targetList ({TARGETENTRY :expr {FUNCEXPR :funcid 480 :funcresulttype 23 :funcretset false :funcvariadic false :funcformat 1 :funccollid 0 :inputcollid 0 :args ({AGGREF :aggfnoid 2803 :aggtype 20 :aggcollid 0 :inputcollid 0 :aggtranstype 0 :aggargtypes <> :aggdirectargs <> :args <> :aggorder <> :aggdistinct <> :aggfilter <> :aggstar true :aggvariadic false :aggkind n :aggpresorted false :agglevelsup 0 :aggsplit 0 :aggno -1 :aggtransno -1 :location 97}) :location 105} :resno 1 :resname count :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location 0 :stmt_len 0}))"##;

pub(crate) const SUBLINK_16: &str = r##"(({QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks true :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :isReturn false :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeUseOuterJoin false :targetList ({TARGETENTRY :expr {OPEXPR :opno 551 :opfuncid 177 :opresulttype 23 :opretset false :opcollid 0 :inputcollid 0 :args ({SUBLINK :subLinkType 4 :subLinkId 0 :testexpr <> :operName <> :subselect {QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :isReturn false :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeUseOuterJoin false :targetList ({TARGETENTRY :expr {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 105 :constvalue 4 [ 1 0 0 0 0 0 0 0 ]} :resno 1 :resname ?column? :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location 0 :stmt_len 0} :location 97} {PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location 110}) :location 108} :resno 1 :resname ?column? :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location 0 :stmt_len 0}))"##;

pub(crate) const SRF_16: &str = r##"(({QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs true :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :isReturn false :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeUseOuterJoin false :targetList ({TARGETENTRY :expr {FUNCEXPR :funcid 1067 :funcresulttype 23 :funcretset true :funcvariadic false :funcformat 0 :funccollid 0 :inputcollid 0 :args ({CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 119 :constvalue 4 [ 1 0 0 0 0 0 0 0 ]} {PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location 122}) :location 103} :resno 1 :resname generate_series :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location 0 :stmt_len 0}))"##;

pub(crate) const RETURN_BODY_18: &str = r##"{QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :hasGroupRTE false :isReturn true :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeTargetRelation 0 :mergeJoinCondition <> :targetList ({TARGETENTRY :expr {OPEXPR :opno 551 :opfuncid 177 :opresulttype 23 :opretset false :opcollid 0 :inputcollid 0 :args ({PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location -1} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ 1 0 0 0 0 0 0 0 ]}) :location -1} :resno 1 :resname <> :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningOldAlias <> :returningNewAlias <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location -1 :stmt_len -1}"##;

pub(crate) const ATOMIC_BODY_18: &str = r##"(({QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :hasGroupRTE false :isReturn false :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeTargetRelation 0 :mergeJoinCondition <> :targetList ({TARGETENTRY :expr {OPEXPR :opno 514 :opfuncid 141 :opresulttype 23 :opretset false :opcollid 0 :inputcollid 0 :args ({PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location -1} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ 2 0 0 0 0 0 0 0 ]}) :location -1} :resno 1 :resname ?column? :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningOldAlias <> :returningNewAlias <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location -1 :stmt_len -1}))"##;

pub(crate) const EMPTY_BODY_18: &str = r##"(<>)"##;

pub(crate) const CALLS_USER_BODY_18: &str = r##"{QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :hasGroupRTE false :isReturn true :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeTargetRelation 0 :mergeJoinCondition <> :targetList ({TARGETENTRY :expr {BOOLEXPR :boolop and :args ({OPEXPR :opno 1209 :opfuncid 850 :opresulttype 16 :opretset false :opcollid 0 :inputcollid 100 :args ({PARAM :paramkind 0 :paramid 1 :paramtype 25 :paramtypmod -1 :paramcollid 100 :location -1} {CONST :consttype 25 :consttypmod -1 :constcollid 100 :constlen -1 :constbyval false :constisnull false :location -1 :constvalue 7 [ 28 0 0 0 37 64 37 ]}) :location -1} {OPEXPR :opno 96 :opfuncid 65 :opresulttype 16 :opretset false :opcollid 0 :inputcollid 0 :args ({FUNCEXPR :funcid 16386 :funcresulttype 23 :funcretset false :funcvariadic false :funcformat 0 :funccollid 0 :inputcollid 0 :args ({CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ 1 0 0 0 0 0 0 0 ]}) :location -1} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ 2 0 0 0 0 0 0 0 ]}) :location -1}) :location -1} :resno 1 :resname <> :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningOldAlias <> :returningNewAlias <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location -1 :stmt_len -1}"##;

pub(crate) const TWO_STATEMENTS_18: &str = r##"(({QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :hasGroupRTE false :isReturn false :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeTargetRelation 0 :mergeJoinCondition <> :targetList ({TARGETENTRY :expr {PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location -1} :resno 1 :resname x :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningOldAlias <> :returningNewAlias <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location -1 :stmt_len -1} {QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :hasGroupRTE false :isReturn false :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeTargetRelation 0 :mergeJoinCondition <> :targetList ({TARGETENTRY :expr {OPEXPR :opno 551 :opfuncid 177 :opresulttype 23 :opretset false :opcollid 0 :inputcollid 0 :args ({PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location -1} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ 1 0 0 0 0 0 0 0 ]}) :location -1} :resno 1 :resname ?column? :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningOldAlias <> :returningNewAlias <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location -1 :stmt_len -1}))"##;

pub(crate) const READS_TABLE_18: &str = r##"(({QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs true :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :hasGroupRTE false :isReturn false :cteList <> :rtable ({RANGETBLENTRY :alias <> :eref {ALIAS :aliasname pg_class :colnames ("oid" "relname" "relnamespace" "reltype" "reloftype" "relowner" "relam" "relfilenode" "reltablespace" "relpages" "reltuples" "relallvisible" "relallfrozen" "reltoastrelid" "relhasindex" "relisshared" "relpersistence" "relkind" "relnatts" "relchecks" "relhasrules" "relhastriggers" "relhassubclass" "relrowsecurity" "relforcerowsecurity" "relispopulated" "relreplident" "relispartition" "relrewrite" "relfrozenxid" "relminmxid" "relacl" "reloptions" "relpartbound")} :rtekind 0 :relid 1259 :inh true :relkind r :rellockmode 1 :perminfoindex 1 :tablesample <> :lateral false :inFromCl true :securityQuals <>}) :rteperminfos ({RTEPERMISSIONINFO :relid 1259 :inh true :requiredPerms 2 :checkAsUser 0 :selectedCols (b 8) :insertedCols (b) :updatedCols (b)}) :jointree {FROMEXPR :fromlist ({RANGETBLREF :rtindex 1}) :quals {OPEXPR :opno 607 :opfuncid 184 :opresulttype 16 :opretset false :opcollid 0 :inputcollid 0 :args ({VAR :varno 1 :varattno 1 :vartype 26 :vartypmod -1 :varcollid 0 :varnullingrels (b) :varlevelsup 0 :varreturningtype 0 :varnosyn 1 :varattnosyn 1 :location -1} {RELABELTYPE :arg {PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location -1} :resulttype 26 :resulttypmod -1 :resultcollid 0 :relabelformat 2 :location -1}) :location -1}} :mergeActionList <> :mergeTargetRelation 0 :mergeJoinCondition <> :targetList ({TARGETENTRY :expr {FUNCEXPR :funcid 480 :funcresulttype 23 :funcretset false :funcvariadic false :funcformat 1 :funccollid 0 :inputcollid 0 :args ({AGGREF :aggfnoid 2803 :aggtype 20 :aggcollid 0 :inputcollid 0 :aggtranstype 0 :aggargtypes <> :aggdirectargs <> :args <> :aggorder <> :aggdistinct <> :aggfilter <> :aggstar true :aggvariadic false :aggkind n :aggpresorted false :agglevelsup 0 :aggsplit 0 :aggno -1 :aggtransno -1 :location -1}) :location -1} :resno 1 :resname count :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningOldAlias <> :returningNewAlias <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location -1 :stmt_len -1}))"##;

pub(crate) const SUBLINK_18: &str = r##"(({QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks true :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :hasGroupRTE false :isReturn false :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeTargetRelation 0 :mergeJoinCondition <> :targetList ({TARGETENTRY :expr {OPEXPR :opno 551 :opfuncid 177 :opresulttype 23 :opretset false :opcollid 0 :inputcollid 0 :args ({SUBLINK :subLinkType 4 :subLinkId 0 :testexpr <> :operName <> :subselect {QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :hasGroupRTE false :isReturn false :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeTargetRelation 0 :mergeJoinCondition <> :targetList ({TARGETENTRY :expr {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ 1 0 0 0 0 0 0 0 ]} :resno 1 :resname ?column? :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningOldAlias <> :returningNewAlias <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location -1 :stmt_len -1} :location -1} {PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location -1}) :location -1} :resno 1 :resname ?column? :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningOldAlias <> :returningNewAlias <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location -1 :stmt_len -1}))"##;

pub(crate) const SRF_18: &str = r##"(({QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs true :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :hasGroupRTE false :isReturn false :cteList <> :rtable <> :rteperminfos <> :jointree {FROMEXPR :fromlist <> :quals <>} :mergeActionList <> :mergeTargetRelation 0 :mergeJoinCondition <> :targetList ({TARGETENTRY :expr {FUNCEXPR :funcid 1067 :funcresulttype 23 :funcretset true :funcvariadic false :funcformat 0 :funccollid 0 :inputcollid 0 :args ({CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ 1 0 0 0 0 0 0 0 ]} {PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location -1}) :location -1} :resno 1 :resname generate_series :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}) :override 0 :onConflict <> :returningOldAlias <> :returningNewAlias <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location -1 :stmt_len -1}))"##;

pub(crate) const MAJORS: [u32; 2] = [16, 18];

fn tree(name: &str, major: u32) -> &'static str {
    match (name, major) {
        ("return", 16) => RETURN_BODY_16,
        ("return", 18) => RETURN_BODY_18,
        ("atomic", 16) => ATOMIC_BODY_16,
        ("atomic", 18) => ATOMIC_BODY_18,
        ("empty", 16) => EMPTY_BODY_16,
        ("empty", 18) => EMPTY_BODY_18,
        ("calls", 16) => CALLS_USER_BODY_16,
        ("calls", 18) => CALLS_USER_BODY_18,
        ("two", 16) => TWO_STATEMENTS_16,
        ("two", 18) => TWO_STATEMENTS_18,
        ("table", 16) => READS_TABLE_16,
        ("table", 18) => READS_TABLE_18,
        ("sublink", 16) => SUBLINK_16,
        ("sublink", 18) => SUBLINK_18,
        ("srf", 16) => SRF_16,
        ("srf", 18) => SRF_18,
        _ => panic!("no tree {name} on {major}"),
    }
}

fn not_plain(text: &str, major: u32) -> String {
    match bound_body(text, major) {
        Err(BodyError::NotPlain(why)) => why,
        other => panic!("expected a body that is not plain on {major}, got {other:?}"),
    }
}

#[test]
fn a_return_body_and_a_begin_atomic_body_bind_their_calls_on_both_majors() {
    for major in MAJORS {
        // `x + 1`: operator 551 (`int4pl`'s `+`), whose function is 177.
        let bound = bound_body(tree("return", major), major).expect("a measured body decodes");
        assert_eq!(bound.operators, BTreeSet::from([551]), "{major}");
        assert_eq!(bound.routines, BTreeSet::from([177]), "{major}");
        assert!(bound.types.contains(&23), "{major}");
        // `x * 2` inside a list of statements: operator 514, function 141.
        let bound = bound_body(tree("atomic", major), major).expect("a measured body decodes");
        assert_eq!(bound.operators, BTreeSet::from([514]), "{major}");
        assert_eq!(bound.routines, BTreeSet::from([141]), "{major}");
        // Every statement of a body counts, not only the last.
        let bound = bound_body(tree("two", major), major).expect("a measured body decodes");
        assert_eq!(bound.operators, BTreeSet::from([551]), "{major}");
        assert_eq!(bound.routines, BTreeSet::from([177]), "{major}");
    }
}

#[test]
fn a_call_of_another_user_function_is_found_in_the_body() {
    for major in MAJORS {
        let bound = bound_body(tree("calls", major), major).expect("a measured body decodes");
        // `LIKE` (850), `=` (65) and the user's own `app.f1`, OID 16386.
        assert!(bound.routines.contains(&16386), "{major}: {bound:?}");
        assert!(bound.routines.contains(&850), "{major}: {bound:?}");
        assert!(bound.routines.contains(&65), "{major}: {bound:?}");
    }
}

#[test]
fn an_empty_body_calls_nothing_and_an_unreadable_one_is_not_that_answer() {
    for major in MAJORS {
        // Empty is its own answer: a body of no statements calls nothing.
        assert_eq!(
            bound_body(tree("empty", major), major),
            Ok(super::Bound::default()),
            "{major}"
        );
        // Negative: text that is not a stored body is unreadable, never empty.
        for text in ["", "garbage", "{NOSUCHNODE :x 1}", "(<> <>)", "(({QUERY}))"] {
            assert!(
                matches!(
                    bound_body(text, major),
                    Err(BodyError::Unreadable(_) | BodyError::NotPlain(_))
                ),
                "`{text}` on {major}"
            );
        }
        let truncated = tree("return", major).replace(" :opcollid 0", "");
        assert!(matches!(
            bound_body(&truncated, major),
            Err(BodyError::Unreadable(_))
        ));
    }
    // A major it was not measured on is refused.
    assert!(matches!(
        bound_body(RETURN_BODY_18, 17),
        Err(BodyError::Unreadable(_))
    ));
}

#[test]
fn a_body_that_is_more_than_plain_expressions_is_refused_by_what_it_adds() {
    for major in MAJORS {
        // A table in FROM: the range table is not empty.
        let why = not_plain(tree("table", major), major);
        assert!(
            why.contains("`hasAggs`") || why.contains("`rtable`"),
            "{major}: {why}"
        );
        // A subquery, and a set-returning function in the target list.
        assert!(not_plain(tree("sublink", major), major).contains("`hasSubLinks`"));
        assert!(not_plain(tree("srf", major), major).contains("`hasTargetSRFs`"));
        // Fields the engine fills only when a clause exists.
        for (from, to, field) in [
            (":sortClause <>", ":sortClause (x)", "`sortClause`"),
            (":limitCount <>", ":limitCount (x)", "`limitCount`"),
            (":cteList <>", ":cteList (x)", "`cteList`"),
            (":groupClause <>", ":groupClause (x)", "`groupClause`"),
            (":hasAggs false", ":hasAggs true", "`hasAggs`"),
            (":resultRelation 0", ":resultRelation 1", "`resultRelation`"),
        ] {
            let text = tree("return", major).replace(from, to);
            assert_ne!(text, tree("return", major), "{from} is in the tree");
            assert!(not_plain(&text, major).contains(field), "{from} on {major}");
        }
        // Not a SELECT, a WHERE clause, a FROM list.
        let text = tree("return", major).replace(":commandType 1", ":commandType 3");
        assert!(not_plain(&text, major).contains("not a SELECT"));
        let text = tree("return", major).replace(":quals <>}", ":quals {CONST}}");
        assert!(not_plain(&text, major).contains("FROM or WHERE"));
        let text =
            tree("return", major).replace(":fromlist <>", ":fromlist ({RANGETBLREF :rtindex 1})");
        assert!(not_plain(&text, major).contains("FROM or WHERE"));
        // Negative: the plain ones are not refused.
        for plain in ["return", "atomic", "two", "calls", "empty"] {
            assert!(
                bound_body(tree(plain, major), major).is_ok(),
                "{plain} on {major}"
            );
        }
    }
}

#[test]
fn a_parameter_is_read_in_a_body_and_nowhere_else() {
    let param =
        "{PARAM :paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location -1}";
    for major in MAJORS {
        // An expression stored for a constraint or a default has no arguments.
        let why = bound_objects(param, major).expect_err("a parameter outside a body");
        assert!(why.contains("PARAM"), "{major}: {why}");
        // Inside a body only the external kind is a copy of a passed value.
        let text = tree("atomic", major);
        assert!(text.contains(":paramkind 0"));
        for kind in [1, 2, 3] {
            let other = text.replace(":paramkind 0", &format!(":paramkind {kind}"));
            let Err(BodyError::Unreadable(why)) = bound_body(&other, major) else {
                panic!("paramkind {kind} on {major}");
            };
            assert!(why.contains("PARAM"), "{major}: {why}");
        }
    }
}

#[test]
fn a_node_without_an_execution_proof_is_refused_inside_a_body_too() {
    for major in MAJORS {
        let text = tree("return", major);
        let param = if major == 16 {
            ":paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location 72"
        } else {
            ":paramkind 0 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location -1"
        };
        let before = format!("{{PARAM {param}}}");
        assert!(text.contains(&before), "{major}");
        let sequence = text.replace(&before, "{NEXTVALUEEXPR :seqid 40001 :typeId 20}");
        let Err(BodyError::Unreadable(why)) = bound_body(&sequence, major) else {
            panic!("a sequence call in a body on {major}");
        };
        assert!(why.contains("NEXTVALUEEXPR"), "{major}: {why}");
    }
}
