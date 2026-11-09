import { CompletionContext, type CompletionResult } from "@codemirror/autocomplete";
import { javascript } from "@codemirror/lang-javascript";
import { language } from "@codemirror/language";
import { EditorState } from "@codemirror/state";
import { describe, expect, it, vi } from "vitest";
import { mongoCompletion, sqlCompartmentConfig } from "./editorExtensions";

const METHODS = ["find", "findOne", "aggregate", "countDocuments", "estimatedDocumentCount", "distinct", "insertOne", "insertMany", "updateOne", "updateMany", "replaceOne", "deleteOne", "deleteMany", "findOneAndUpdate", "findOneAndReplace", "findOneAndDelete", "bulkWrite", "drop", "createIndex", "dropIndex", "getIndexes", "renameCollection"];
const OPERATORS = (
  "$eq $ne $gt $gte $lt $lte $in $nin $exists $type $regex $and $or $nor $not $elemMatch $size $all $expr " +
  "$set $unset $inc $push $pull $addToSet $rename " +
  "$match $group $project $sort $limit $skip $unwind $lookup $count $addFields $out $merge " +
  "$sum $avg $min $max $first $last"
).split(" ");
const CONSTRUCTORS = ["ObjectId", "ISODate", "NumberLong", "NumberInt", "NumberDecimal", "UUID"];

/** Completes at the `|` in `doc` (the bar is removed). */
function complete(doc: string, explicit = false, collections = ["users", "orders"]): CompletionResult | null {
  const pos = doc.indexOf("|");
  const text = doc.replace("|", "");
  const state = EditorState.create({ doc: text });
  return mongoCompletion(collections)(new CompletionContext(state, pos, explicit)) as CompletionResult | null;
}
const labels = (r: CompletionResult | null) => (r?.options ?? []).map((o) => o.label);

/** As `complete`, in a state with the tab's JavaScript language, so the syntax tree is there. */
function completeJs(doc: string, explicit = false, collections = ["users", "orders"]): CompletionResult | null {
  const pos = doc.indexOf("|");
  const state = EditorState.create({ doc: doc.replace("|", ""), extensions: [javascript()] });
  return mongoCompletion(collections)(new CompletionContext(state, pos, explicit)) as CompletionResult | null;
}

describe("mongoCompletion", () => {
  it("after db. offers the collections and getCollection", () => {
    expect(labels(complete("db.|"))).toEqual(["users", "orders", "getCollection"]);
    expect(labels(complete("db.us|"))).toContain("users");
  });

  it("after db.<coll>. offers the methods", () => {
    expect(labels(complete("db.users.|"))).toEqual(METHODS);
  });

  it("after db.getCollection(\"…\"). and db[\"…\"]. offers the methods", () => {
    expect(labels(complete('db.getCollection("my coll").|'))).toEqual(METHODS);
    expect(labels(complete('db["my coll"].|'))).toEqual(METHODS);
    expect(labels(complete("db.getCollection('a').fi|"))).toEqual(METHODS);
  });

  it("after ). in a find chain offers sort, skip and limit", () => {
    expect(labels(complete("db.users.find({}).|"))).toEqual(["sort", "skip", "limit"]);
    expect(labels(complete('db.users.find({ a: 1 }).sort({ a: 1 }).|'))).toEqual(["sort", "skip", "limit"]);
    expect(labels(complete("db.users.find({})\n  .|"))).toEqual(["sort", "skip", "limit"]);
  });

  it("after ). outside a find chain offers nothing", () => {
    expect(complete("db.users.aggregate([]).|")).toBeNull();
    // An earlier find statement does not make the next one a find chain.
    expect(complete("db.users.find({});\ndb.users.aggregate([]).|")).toBeNull();
  });

  it("a word starting with $ offers the operators", () => {
    expect(labels(complete("db.users.find({ a: { $|"))).toEqual(OPERATORS);
    const r = complete("db.users.find({ a: { $gt|");
    expect(r?.from).toBe("db.users.find({ a: { ".length);
    expect(labels(r)).toEqual(OPERATORS);
  });

  it("otherwise offers the constructors", () => {
    expect(labels(complete("db.users.find({ _id: Obj|"))).toEqual(CONSTRUCTORS);
    expect(labels(complete("db.users.find({ _id: |", true))).toEqual(CONSTRUCTORS);
  });

  it("offers nothing on an empty word that was not asked for, or after another dot", () => {
    expect(complete("db.users.find({ _id: |")).toBeNull();
    expect(complete("x.|")).toBeNull();
  });
});

describe("the editor language for mongodb", () => {
  it("is JavaScript, not SQL", () => {
    const lang = (kind: "mongodb" | "postgres") => EditorState.create({ extensions: [sqlCompartmentConfig({ users: [] }, "fx", kind)] }).facet(language)?.name;
    expect(lang("mongodb")).toBe("javascript");
    expect(lang("postgres")).toBe("sql");
  });
});

describe("mongoCompletion with the syntax tree", () => {
  it("offers nothing inside a string, a comment or a regex", () => {
    for (const doc of [
      'db.users.find({ name: "$|',
      'db.users.find({ name: "$x|" })',
      "db.users.find({ name: '$|' })",
      "db.users.find({ name: `$|` })",
      "db.users.find({ name: /x$|/ })",
      "// db.|",
      "// db.users.|",
      "db.a.find() /* db.| */",
      'db.users.find({ name: "Obj|" })',
    ]) {
      expect(completeJs(doc), doc).toBeNull();
      expect(completeJs(doc, true), `${doc} (asked)`).toBeNull();
    }
  });

  it("completes again right after a closed string or comment", () => {
    expect(labels(completeJs('db.users.find({ a: "x" }, { $|'))).toEqual(OPERATORS);
    expect(labels(completeJs("/* note */ db.|"))).toEqual(["users", "orders", "getCollection"]);
    expect(labels(completeJs('db.getCollection("users").|'))).toEqual(METHODS);
  });

  it("does not offer the find chain inside the find's arguments", () => {
    expect(completeJs('db.users.find({ _id: ObjectId("x").|')).toBeNull();
    expect(completeJs('db.users.find({ _id: ObjectId("x").| })')).toBeNull();
    expect(labels(completeJs('db.users.find({ _id: ObjectId("x") }).|'))).toEqual(["sort", "skip", "limit"]);
  });

  it("keeps the other contexts", () => {
    expect(labels(completeJs("db.|"))).toEqual(["users", "orders", "getCollection"]);
    expect(labels(completeJs("db.users.|"))).toEqual(METHODS);
    expect(labels(completeJs("db.users.find({}).|"))).toEqual(["sort", "skip", "limit"]);
    expect(labels(completeJs("db.users.find({})\n  .sort({ a: 1 }).|"))).toEqual(["sort", "skip", "limit"]);
    expect(completeJs("db.users.aggregate([]).|")).toBeNull();
    expect(labels(completeJs("db.users.find({ a: { $|"))).toEqual(OPERATORS);
    expect(labels(completeJs("db.users.find({ _id: Obj|"))).toEqual(CONSTRUCTORS);
    // Before the completion map has loaded, db. offers getCollection alone.
    expect(labels(completeJs("db.|", false, []))).toEqual(["getCollection"]);
  });
});

/** As `completeJs`, with a `fields` loader; awaits a result that comes as a promise. */
async function completeFields(doc: string, fields: (collection: string) => Promise<string[]>, explicit = false): Promise<CompletionResult | null> {
  const pos = doc.indexOf("|");
  const state = EditorState.create({ doc: doc.replace("|", ""), extensions: [javascript()] });
  return await mongoCompletion(["users", "orders"], fields)(new CompletionContext(state, pos, explicit));
}

describe("mongoCompletion of field names", () => {
  const FIELDS = ["_id", "name", "address", "address.city", "age"];
  const fake = () => vi.fn((c: string) => Promise.resolve(c === "users" ? FIELDS : []));

  it("offers the collection's fields at a key of a call's object argument", async () => {
    const fields = fake();
    const r = await completeFields("db.users.find({ na|", fields);
    expect(labels(r)).toEqual(FIELDS);
    expect(fields).toHaveBeenCalledWith("users");
    expect(r?.from).toBe("db.users.find({ ".length);
    expect(r?.options.every((o) => o.type === "property")).toBe(true);
    expect(labels(await completeFields('db.users.find({ name: "Ann", a|', fields))).toEqual(FIELDS);
    expect(labels(await completeFields("db.users.updateOne({ _id: 1 }, { $set: { ag|", fields))).toEqual(FIELDS);
    expect(labels(await completeFields("db.users.aggregate([{ $match: { na|", fields))).toEqual(FIELDS);
    expect(labels(await completeFields("db.users.find({\n  name: 1,\n  a|\n})", fields))).toEqual(FIELDS);
    expect(labels(await completeFields("db.users.find({ a: 1, |", fields, true))).toEqual(FIELDS);
  });

  it("takes the collection from getCollection and db[...]", async () => {
    const fields = fake();
    expect(labels(await completeFields('db.getCollection("users").find({ na|', fields))).toEqual(FIELDS);
    expect(fields).toHaveBeenLastCalledWith("users");
    expect(labels(await completeFields("db['users'].find({ na|", fields))).toEqual(FIELDS);
    expect(fields).toHaveBeenLastCalledWith("users");
    await completeFields("db.users.find({});\ndb.orders.find({ na|", fields);
    expect(fields).toHaveBeenLastCalledWith("orders");
  });

  it("applies a path quoted and a plain name as itself", async () => {
    const r = await completeFields("db.users.find({ ci|", fake());
    const option = (label: string) => r?.options.find((o) => o.label === label);
    expect(option("address.city")?.apply).toBe('"address.city"');
    expect(option("name")).toBeDefined();
    expect(option("name")?.apply).toBeUndefined();
  });

  it("keeps the constructors at a value and the operators on $", async () => {
    const fields = fake();
    expect(labels(await completeFields("db.users.find({ name: na|", fields))).toEqual(CONSTRUCTORS);
    expect(labels(await completeFields("db.users.find({ $|", fields))).toEqual(OPERATORS);
    expect(labels(await completeFields("db.users.find({ a: [ na|", fields))).toEqual(CONSTRUCTORS);
    expect(labels(await completeFields("db.users.find({ a: 1 }, na|", fields))).toEqual(CONSTRUCTORS);
    expect(fields).not.toHaveBeenCalled();
  });

  it("offers nothing inside a string", async () => {
    const fields = fake();
    expect(await completeFields('db.users.find({ "na|', fields)).toBeNull();
    expect(await completeFields('db.users.find({ "na|', fields, true)).toBeNull();
    expect(fields).not.toHaveBeenCalled();
  });

  it("offers nothing, without throwing, when there are no fields or they fail to load", async () => {
    expect(await completeFields("db.orders.find({ na|", fake())).toBeNull();
    expect(await completeFields("db.users.find({ na|", () => Promise.reject(new Error("down")))).toBeNull();
    expect(await completeFields("db.users.find({ na|", () => { throw new Error("sync"); })).toBeNull();
  });

  it("leaves completion after db. and after ). as it was", async () => {
    const fields = fake();
    expect(labels(await completeFields("db.|", fields))).toEqual(["users", "orders", "getCollection"]);
    expect(labels(await completeFields("db.users.|", fields))).toEqual(METHODS);
    expect(labels(await completeFields("db.users.find({}).|", fields))).toEqual(["sort", "skip", "limit"]);
    expect(fields).not.toHaveBeenCalled();
  });
});
