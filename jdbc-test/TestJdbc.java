import java.sql.*;
import java.util.Properties;

public class TestJdbc {
    static final String HOST = System.getenv().getOrDefault("MYDB_HOST", "localhost");
    static final String PORT = System.getenv().getOrDefault("MYDB_PORT", "3306");
    static final String USER = "root";
    static final String PASS = "root";
    static final String BASE_URL = "jdbc:mysql://" + HOST + ":" + PORT;

    static int totalTests = 0;
    static int passedTests = 0;

    static void tryConnect(String label, String url, Properties props, boolean doDML) {
        totalTests++;
        System.out.println("===== TEST " + totalTests + ": " + label + " =====");
        System.out.println("URL: " + url);
        System.out.println("Properties: " + props);
        try (Connection c = DriverManager.getConnection(url, props)) {
            System.out.println("RESULT: CONNECTED OK");
            System.out.println("Product: " + c.getMetaData().getDatabaseProductName());
            System.out.println("Version: " + c.getMetaData().getDatabaseProductVersion());
            System.out.println("Driver: " + c.getMetaData().getDriverName() + " " + c.getMetaData().getDriverVersion());
            System.out.println("AutoCommit: " + c.getAutoCommit());
            System.out.println("TransactionIsolation: " + c.getTransactionIsolation());

            // Basic SELECT 1 test
            boolean basicOk = false;
            try (Statement s = c.createStatement();
                 ResultSet rs = s.executeQuery("SELECT 1")) {
                if (rs.next()) {
                    int val = rs.getInt(1);
                    System.out.println("QUERY SELECT 1 = " + val);
                    if (val == 1) {
                        System.out.println("VERIFICATION: SELECT 1 returned correct value");
                        basicOk = true;
                    } else {
                        System.out.println("VERIFICATION FAILED: Expected 1, got " + val);
                    }
                } else {
                    System.out.println("VERIFICATION FAILED: ResultSet is empty");
                }
            }

            if (!basicOk) {
                System.out.println("RESULT: BASIC TEST FAILED - skipping DML");
                System.out.println();
                return;
            }

            if (doDML) {
                // Test CREATE DATABASE and USE
                try (Statement s = c.createStatement()) {
                    s.execute("CREATE DATABASE IF NOT EXISTS test_jdbc");
                    System.out.println("VERIFICATION: CREATE DATABASE succeeded");
                } catch (SQLException e) {
                    System.out.println("CREATE DATABASE issue: " + e.getMessage());
                }

                // Test DROP/CREATE TABLE to be idempotent
                try (Statement s = c.createStatement()) {
                    s.execute("DROP TABLE IF EXISTS test_jdbc.t1");
                    s.execute("CREATE TABLE test_jdbc.t1 (id INT PRIMARY KEY, name VARCHAR(100))");
                    System.out.println("VERIFICATION: DROP/CREATE TABLE succeeded");
                } catch (SQLException e) {
                    System.out.println("CREATE TABLE issue: " + e.getMessage());
                }

                // Test INSERT
                try (Statement s = c.createStatement()) {
                    int rows = s.executeUpdate("INSERT INTO test_jdbc.t1 VALUES (1, 'hello'), (2, 'world')");
                    System.out.println("VERIFICATION: INSERT affected " + rows + " rows");
                } catch (SQLException e) {
                    System.out.println("INSERT issue: " + e.getMessage());
                }

                // Test SELECT
                try (Statement s = c.createStatement();
                     ResultSet rs = s.executeQuery("SELECT id, name FROM test_jdbc.t1 ORDER BY id")) {
                    int count = 0;
                    while (rs.next()) {
                        int id = rs.getInt(1);
                        String name = rs.getString(2);
                        System.out.println("  ROW: id=" + id + ", name=" + name);
                        count++;
                    }
                    if (count == 2) {
                        System.out.println("VERIFICATION: SELECT returned " + count + " rows");
                    } else {
                        System.out.println("VERIFICATION FAILED: Expected 2 rows, got " + count);
                    }
                } catch (SQLException e) {
                    System.out.println("SELECT issue: " + e.getMessage());
                }
            }

            // Test session variables (JDBC often queries these)
            try (Statement s = c.createStatement();
                 ResultSet rs = s.executeQuery("SELECT @@autocommit, @@version_comment, @@transaction_isolation")) {
                if (rs.next()) {
                    Object ac = rs.getObject(1);
                    Object vc = rs.getObject(2);
                    Object ti = rs.getObject(3);
                    System.out.println("@@autocommit=" + ac + ", @@version_comment=" + vc + ", @@transaction_isolation=" + ti);
                }
            } catch (SQLException e) {
                System.out.println("Session vars issue: " + e.getMessage());
            }

            passedTests++;

        } catch (SQLException e) {
            System.out.println("RESULT: FAILED");
            System.out.println("SQLState: " + e.getSQLState());
            System.out.println("Vendor Error Code: " + e.getErrorCode());
            System.out.println("Message: " + e.getMessage());
            Throwable t = e.getCause();
            int depth = 0;
            while (t != null) {
                System.out.println("  Cause[" + depth + "]: " + t.getClass().getName() + ": " + t.getMessage());
                t = t.getCause();
                depth++;
            }
            e.printStackTrace(System.out);
        }
        System.out.println();
    }

    static Properties baseProps() {
        Properties p = new Properties();
        p.setProperty("user", USER);
        p.setProperty("password", PASS);
        p.setProperty("useSSL", "false");
        p.setProperty("allowPublicKeyRetrieval", "true");
        p.setProperty("serverTimezone", "UTC");
        p.setProperty("connectTimeout", "10000");
        p.setProperty("socketTimeout", "30000");
        return p;
    }

    public static void main(String[] args) throws Exception {
        Class.forName("com.mysql.cj.jdbc.Driver");
        System.out.println("=== MySQL Connector/J 8.0.x JDBC Test for MyDB ===");
        System.out.println("Target: " + BASE_URL);
        System.out.println("Username: " + USER);
        System.out.println();

        // Test 1: Default (caching_sha2_password -> auth switch to mysql_native_password)
        Properties p1 = baseProps();
        tryConnect("Default auth (caching_sha2_password -> mysql_native_password)",
                BASE_URL + "/", p1, true);

        // Test 2: Explicit mysql_native_password
        Properties p2 = baseProps();
        p2.setProperty("defaultAuthenticationPlugin", "com.mysql.cj.protocol.a.authentication.MysqlNativePasswordPlugin");
        tryConnect("Explicit mysql_native_password plugin",
                BASE_URL + "/", p2, false);

        // Test 3: With character encoding
        Properties p3 = baseProps();
        p3.setProperty("characterEncoding", "UTF-8");
        p3.setProperty("useUnicode", "true");
        tryConnect("With UTF-8 character encoding",
                BASE_URL + "/", p3, false);

        // Test 4: Common GUI tool properties (like IDEA/DataGrip)
        Properties p4 = baseProps();
        p4.setProperty("tinyInt1isBit", "false");
        p4.setProperty("zeroDateTimeBehavior", "CONVERT_TO_NULL");
        p4.setProperty("useInformationSchema", "true");
        tryConnect("With common GUI tool properties (DataGrip/IDEA-like)",
                BASE_URL + "/", p4, false);

        System.out.println("=========================================");
        System.out.println("SUMMARY: " + passedTests + "/" + totalTests + " tests passed");
        if (passedTests == totalTests) {
            System.out.println("ALL TESTS PASSED");
            System.exit(0);
        } else {
            System.out.println("SOME TESTS FAILED");
            System.exit(1);
        }
    }
}
